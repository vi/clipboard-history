use std::{
    mem::MaybeUninit,
    path::PathBuf,
    str::FromStr,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    thread,
    thread::JoinHandle,
};

use arrayvec::ArrayVec;
use memchr::memmem::Finder;
use regex::bytes::Regex;
use ringboard_core::{
    bucket_to_length,
    ring::{Mmap, MAX_ENTRIES},
    size_to_bucket, IoErr, TEXT_MIMES,
};
use rustix::{
    fs::{openat, Mode, OFlags, RawDir},
    path::Arg,
    thread::{unshare, UnshareFlags},
};

use crate::{ring_reader::xattr_mime_type, EntryReader};

#[derive(Clone, Debug)]
pub enum Query<'a> {
    Plain(&'a [u8]),
    Regex(Regex),
}

trait QueryImpl {
    fn find(&self, haystack: &[u8]) -> Option<(usize, usize)>;

    fn needle_len(&self) -> Option<usize>;
}

#[derive(Clone)]
struct PlainQuery(Arc<Finder<'static>>);

impl QueryImpl for PlainQuery {
    fn find(&self, haystack: &[u8]) -> Option<(usize, usize)> {
        self.0
            .find(haystack)
            .map(|start| (start, start + self.0.needle().len()))
    }

    fn needle_len(&self) -> Option<usize> {
        Some(self.0.needle().len())
    }
}

#[derive(Clone)]
struct RegexQuery(Regex);

impl QueryImpl for RegexQuery {
    fn find(&self, haystack: &[u8]) -> Option<(usize, usize)> {
        self.0.find(haystack).map(|m| (m.start(), m.end()))
    }

    fn needle_len(&self) -> Option<usize> {
        None
    }
}

#[derive(Copy, Clone, Debug)]
pub struct QueryResult {
    pub location: EntryLocation,
    pub start: usize,
    pub end: usize,
}

#[derive(Copy, Clone, Debug)]
pub enum EntryLocation {
    Bucketed { bucket: u8, index: u32 },
    File { entry_id: u64 },
}

#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct BucketAndIndex(u32);

impl BucketAndIndex {
    pub fn new(bucket: u8, index: u32) -> Self {
        debug_assert!(index <= MAX_ENTRIES);
        Self((index << u8::BITS) | u32::from(bucket))
    }

    pub fn bucket(&self) -> u8 {
        u8::try_from(self.0 & u32::from(u8::MAX)).unwrap()
    }

    pub fn index(&self) -> u32 {
        self.0 >> u8::BITS
    }
}

struct QueryIter {
    stream: mpsc::IntoIter<Result<QueryResult, ringboard_core::Error>>,
    stop: Arc<AtomicBool>,
}

impl Iterator for QueryIter {
    type Item = Result<QueryResult, ringboard_core::Error>;

    fn next(&mut self) -> Option<Self::Item> {
        self.stream.next()
    }
}

impl Drop for QueryIter {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

pub fn search(
    query: Query,
    reader: Arc<EntryReader>,
) -> (
    impl Iterator<Item = Result<QueryResult, ringboard_core::Error>>,
    impl Iterator<Item = JoinHandle<()>>,
) {
    let (results, threads) = match query {
        Query::Plain(p) => search_impl(PlainQuery(Arc::new(Finder::new(p).into_owned())), reader),
        Query::Regex(r) => search_impl(RegexQuery(r), reader),
    };
    (results, threads.into_iter())
}

fn search_impl(
    query: impl QueryImpl + Clone + Send + 'static,
    reader: Arc<EntryReader>,
) -> (QueryIter, arrayvec::IntoIter<JoinHandle<()>, 12>) {
    let (sender, receiver) = mpsc::sync_channel(0);
    let stop = Arc::new(AtomicBool::new(false));
    let mut threads = ArrayVec::<_, 12>::new_const();

    for bucket in usize::from(size_to_bucket(
        u32::try_from(query.needle_len().unwrap_or(0)).unwrap_or(u32::MAX),
    ))..reader.buckets().len()
    {
        let query = query.clone();
        let reader = reader.clone();
        let sender = sender.clone();
        let stop = stop.clone();
        threads.push(thread::spawn(move || {
            for (index, entry) in reader.buckets()[bucket]
                .chunks_exact(usize::try_from(bucket_to_length(bucket)).unwrap())
                .enumerate()
            {
                if stop.load(Ordering::Relaxed) {
                    break;
                }

                let entry = if let Some(stop) = memchr::memchr(0, entry) {
                    &entry[..stop]
                } else {
                    entry
                };
                let Some((start, end)) = query.find(entry) else {
                    continue;
                };
                if sender
                    .send(Ok(QueryResult {
                        location: EntryLocation::Bucketed {
                            bucket: u8::try_from(bucket).unwrap(),
                            index: u32::try_from(index).unwrap(),
                        },
                        start,
                        end,
                    }))
                    .is_err()
                {
                    break;
                }
            }
        }));
    }
    threads.push(thread::spawn({
        let stop = stop.clone();
        move || {
            let direct_dir = match openat(reader.direct(), c".", OFlags::DIRECTORY, Mode::empty())
                .map_io_err(|| "Failed to open direct dir.")
                .and_then(|fd| {
                    unshare(UnshareFlags::FILES | UnshareFlags::FS)
                        .map_io_err(|| "Failed to unshare I/O.")?;
                    Ok(fd)
                }) {
                Ok(fd) => fd,
                Err(e) => {
                    let _ = sender.send(Err(e));
                    return;
                }
            };

            let mut buf = [MaybeUninit::uninit(); 8192];
            let mut iter = RawDir::new(&direct_dir, &mut buf);
            while let Some(file) = iter.next() {
                if stop.load(Ordering::Relaxed) {
                    break;
                }

                if match file
                    .map_io_err(|| "Failed to read direct allocation directory.")
                    .and_then(|file| {
                        {
                            let name = file.file_name();
                            if name == c"." || name == c".." {
                                return Ok(None);
                            }
                        }

                        let fd =
                            openat(&direct_dir, file.file_name(), OFlags::RDONLY, Mode::empty())
                                .map_io_err(|| {
                                    format!(
                                        "Failed to open direct allocation: {:?}",
                                        file.file_name().to_string_lossy()
                                    )
                                })?;
                        let mime_type = xattr_mime_type(&fd)?;
                        if !mime_type.is_empty() || !TEXT_MIMES.contains(&&*mime_type) {
                            return Ok(None);
                        }

                        let bytes =
                            Mmap::from(&fd).map_io_err(|| "Failed to mmap direct allocation.")?;
                        let Some((start, end)) = query.find(&bytes) else {
                            return Ok(None);
                        };

                        let id = file
                            .file_name()
                            .to_bytes()
                            .split_once(|&b| b == b'_')
                            .and_then(|(ring, entry)| {
                                Some(
                                    (u64::from(u32::from_str(ring.as_str().ok()?).ok()?) << 32)
                                        | u64::from(u32::from_str(entry.as_str().ok()?).ok()?),
                                )
                            })
                            .ok_or(ringboard_core::Error::NotARingboard {
                                file: PathBuf::from(&*file.file_name().to_string_lossy()),
                            })?;

                        Ok(Some(QueryResult {
                            location: EntryLocation::File { entry_id: id },
                            start,
                            end,
                        }))
                    }) {
                    Ok(Some(r)) => sender.send(Ok(r)),
                    Ok(None) => continue,
                    Err(e) => sender.send(Err(e)),
                }
                .is_err()
                {
                    break;
                }
            }
        }
    }));

    (
        QueryIter {
            stream: receiver.into_iter(),
            stop,
        },
        threads.into_iter(),
    )
}
