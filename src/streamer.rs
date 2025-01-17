use crate::other;
#[cfg(unix)]
use std::os::unix::prelude::*;

#[cfg(windows)]
use std::os::windows::prelude::*;

use std::collections::HashMap;
use std::fs::{self};
use std::io::{self, Read, Result, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::str;

use crate::header::{prepare_header, Header, HeaderMode};
#[cfg(windows)]
use crate::other;
use crate::EntryType;

struct StreamFile {
    path: PathBuf,
    alternative_name: Option<PathBuf>,
    follow: bool,
    mode: HeaderMode,
    cached_header_bytes: Option<Vec<u8>>,
    read_bytes: usize, //needed to calculate padding;
    padding_bytes: Option<Vec<u8>>,
}

impl StreamFile {
    fn new(
        path: PathBuf,
        alternative_name: Option<PathBuf>,
        follow: bool,
        mode: HeaderMode,
    ) -> Self {
        Self {
            path,
            alternative_name,
            follow,
            mode,
            cached_header_bytes: None, //will be encoded while reading (to save memory)
            read_bytes: 0,
            padding_bytes: None, //will be calculated while using io::Read implementation.
        }
    }
}

struct StreamData {
    encoded_header: Vec<u8>,
    data: Box<dyn Read + Send>,
    padding_bytes: Option<Vec<u8>>,
    read_bytes: usize, //needed to calculate padding;
}

impl StreamData {
    fn new<R: Read + 'static + Send>(header: Header, data: R) -> Self {
        Self {
            encoded_header: header.as_bytes().to_vec(),
            data: Box::new(data),
            padding_bytes: None, //will be calculated while using io::Read implementation.
            read_bytes: 0,
        }
    }

    fn new_with_encoded_header<R: Read + 'static + Send>(encoded_header: Vec<u8>, data: R) -> Self {
        Self {
            encoded_header,
            data: Box::new(data),
            padding_bytes: None,
            read_bytes: 0,
        }
    }
}

#[cfg(unix)]
struct StreamSpecialFile {
    cached_header_bytes: Option<Vec<u8>>,
    path: PathBuf,
    mode: HeaderMode,
    follow: bool,
}

#[cfg(unix)]
impl StreamSpecialFile {
    fn new<P: AsRef<Path>>(path: P, mode: HeaderMode, follow: bool) -> Self {
        Self {
            cached_header_bytes: None,
            path: path.as_ref().into(),
            mode,
            follow,
        }
    }
}

struct StreamLink {
    encoded_header: Vec<u8>,
}

impl StreamLink {
    fn new_with_encoded_header(encoded_header: Vec<u8>) -> Self {
        Self { encoded_header }
    }
}

struct StreamerReadMetadata {
    read_bytes: usize,
    current_index: usize,
    finish_bytes_remaining: usize,
}

impl Default for StreamerReadMetadata {
    fn default() -> Self {
        Self {
            read_bytes: 0,
            current_index: 0,
            finish_bytes_remaining: 1024,
        }
    }
}

/// A structure for building and streaming archives.
///
/// This structure has methods for building up an archive from scratch and implements [std::io::Read] for this archive.
/// You have not to provide an [io::Write]r, you can just read directly from the [Streamer].
/// It works like a [Builder], just as a [io::Read]er.
/// The archive will "auto-finish" while reading.
///
/// # Example usage
/// ```
/// use std::path::PathBuf;
/// use std::fs;
/// use tar::Streamer;
/// use std::io;
///
///  let mut streamer = Streamer::new();
///  // Use the directory at one location, but insert it into the archive
///  // with a different name.
///  streamer.append_dir_all(&PathBuf::from("my_download_dir"),
///  &PathBuf::from("/home/ph0llux/Downloads")).unwrap();
///  // Write the archive to the given path.
///  let mut target_archive = fs::File::create("/home/ph0llux/my_downloads.tar").unwrap();
///  io::copy(&mut streamer, &mut target_archive).unwrap();
///  ```
pub struct Streamer {
    mode: HeaderMode,
    follow: bool,
    streamer_metadata: StreamerReadMetadata,
    index_counter: usize,
    stream_files: HashMap<usize, StreamFile>, // <index_counter, StreamFile>
    stream_data: HashMap<usize, StreamData>,  // <index_counter, StreamData>
    stream_special_file: HashMap<usize, StreamSpecialFile>, //<index_counter, StreamSpecialFile>
    stream_link: HashMap<usize, StreamLink>,  // <index_counter, StreamLink>
}

impl Default for Streamer {
    fn default() -> Self {
        Self::new()
    }
}

impl Streamer {
    /// Creates a new empty archive streamer. The streamer will use
    /// `HeaderMode::Complete` by default.
    pub fn new() -> Streamer {
        Self {
            mode: HeaderMode::Complete,
            follow: true,
            streamer_metadata: StreamerReadMetadata::default(),
            index_counter: 0,
            stream_files: HashMap::new(),
            stream_data: HashMap::new(),
            stream_special_file: HashMap::new(),
            stream_link: HashMap::new(),
        }
    }

    /// Changes the HeaderMode that will be used when reading fs Metadata for
    /// methods that implicitly read metadata for an input Path. Notably, this
    /// does _not_ apply to `append(Header)`.
    pub fn mode(&mut self, mode: HeaderMode) {
        self.mode = mode;
    }

    /// Follow symlinks, archiving the contents of the file they point to rather
    /// than adding a symlink to the archive. Defaults to true.
    pub fn follow_symlinks(&mut self, follow: bool) {
        self.follow = follow;
    }

    /// Adds a new entry to the archive.
    ///
    /// This function will append the header specified, followed by contents of
    /// the stream specified by `data`. To produce a valid archive the `size`
    /// field of `header` must be the same as the length of the stream that's
    /// being written. Additionally the checksum for the header should have been
    /// set via the `set_cksum` method.
    ///
    /// # Examples
    ///
    /// ```
    /// use tar::{Streamer, Header};
    /// use std::io;
    /// use std::fs;
    ///
    /// let mut header = Header::new_gnu();
    /// header.set_path("foo").unwrap();
    /// header.set_size(4);
    /// header.set_cksum();
    ///
    /// let mut data: &[u8] = &[1, 2, 3, 4];
    ///
    /// let mut ar = Streamer::new();
    /// ar.append(header, data);
    /// let mut output_file = fs::File::create("my_archive.tar").unwrap();
    /// io::copy(&mut ar, &mut output_file);
    /// ```
    pub fn append<R: Read + 'static + Send>(&mut self, header: Header, data: R) {
        let stream_data = StreamData::new(header, data);
        self.stream_data.insert(self.index_counter, stream_data);
        self.index_counter += 1;
    }

    /// Adds a new entry to this archive with the specified path.
    ///
    /// This function will set the specified path in the given header, which may
    /// require appending a GNU long-name extension entry to the archive first.
    /// The checksum for the header will be automatically updated via the
    /// `set_cksum` method after setting the path. No other metadata in the
    /// header will be modified.
    ///
    /// Then it will append the header, followed by contents of the stream
    /// specified by `data`. To produce a valid archive the `size` field of
    /// `header` must be the same as the length of the stream that's being
    /// read.
    ///
    /// # Errors
    ///
    /// This function will return an error for any intermittent I/O error which
    /// occurs while trying to add new streams to the archive.
    ///
    /// # Examples
    ///
    /// ```
    /// use tar::{Streamer, Header};
    /// use std::io;
    /// use std::fs;
    ///
    /// let mut header = Header::new_gnu();
    /// header.set_size(4);
    /// header.set_cksum();
    ///
    /// let mut data: &[u8] = &[1, 2, 3, 4];
    ///
    /// let mut ar = Streamer::new();
    /// ar.append_data(&mut header, "really/long/path/to/foo", data).unwrap();
    /// let mut output_file = fs::File::create("my_archive.tar").unwrap();
    /// io::copy(&mut ar, &mut output_file);
    /// ```
    pub fn append_data<P: AsRef<Path>, R: Read + 'static + Send>(
        &mut self,
        header: &mut Header,
        path: P,
        data: R,
    ) -> Result<()> {
        let mut encoded_header = Vec::new();
        if let Some(mut long_name_extension_entry) = prepare_header_path(header, path.as_ref())? {
            encoded_header.append(&mut long_name_extension_entry);
            //self.long_name_extension_entries.insert(self.index_counter, long_name_extension_entry);
        }
        header.set_cksum();
        encoded_header.append(&mut header.as_bytes().to_vec());
        self.append_stream_data(StreamData::new_with_encoded_header(encoded_header, data));
        Ok(())
    }

    /// Adds a new link (symbolic or hard) entry to this archive with the specified path and target.
    ///
    /// This function is similar to [`Self::append_data`] which supports long filenames,
    /// but also supports long link targets using GNU extensions if necessary.
    /// You must set the entry type to either [`EntryType::Link`] or [`EntryType::Symlink`].
    /// The `set_cksum` method will be invoked after setting the path. No other metadata in the
    /// header will be modified.
    ///
    /// If you are intending to use GNU extensions, you must use this method over calling
    /// [`Header::set_link_name`] because that function will fail on long links.
    ///
    /// Similar constraints around the position of the archive and completion
    /// apply as with [`Self::append_data`].
    ///
    /// # Errors
    ///
    /// This function will return an error for any intermittent I/O error which
    /// occurs when trying to add the link to the archive.
    ///
    /// # Examples
    ///
    /// ```
    /// use tar::{Streamer, Header, EntryType};
    /// use std::io;
    /// use std::fs;
    ///
    /// let mut ar = Streamer::new();
    /// let mut header = Header::new_gnu();
    /// header.set_username("foo");
    /// header.set_entry_type(EntryType::Symlink);
    /// header.set_size(0);
    /// ar.append_link(&mut header, "really/long/path/to/foo", "other/really/long/target").unwrap();
    /// let mut output_file = fs::File::create("my_archive.tar").unwrap();
    /// io::copy(&mut ar, &mut output_file);
    /// ```
    pub fn append_link<P: AsRef<Path>, T: AsRef<Path>>(
        &mut self,
        header: &mut Header,
        path: P,
        target: T,
    ) -> io::Result<()> {
        let mut encoded_header = Vec::new();
        if let Some(mut long_name_extension_entry) = prepare_header_path(header, path.as_ref())? {
            encoded_header.append(&mut long_name_extension_entry);
        }
        if let Some(mut long_name_extension_entry) = prepare_header_link(header, target.as_ref())? {
            encoded_header.append(&mut long_name_extension_entry)
        };
        header.set_cksum();
        encoded_header.append(&mut header.as_bytes().to_vec());
        self.stream_link.insert(
            self.index_counter,
            StreamLink::new_with_encoded_header(encoded_header),
        );
        self.index_counter += 1;
        Ok(())
    }

    /// Adds a file on the local filesystem to this archive.
    ///
    /// This function will only add the given path of the file specified by `path`
    /// to the archive. Any I/O error which orrcurs while opening the file
    /// or reading the appropriate metadata will be returning while using
    /// an reading the archive.  
    /// The path name for the file inside of this archive will be the same as `path`,
    /// and it is required that the path is a relative path.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use tar::Streamer;
    ///
    /// let mut ar = Streamer::new();
    ///
    /// ar.append_path("foo/bar.txt").unwrap();
    /// ```
    pub fn append_path<P: AsRef<Path>>(&mut self, path: P) -> io::Result<()> {
        self.append_stream_file(path.as_ref(), None)
    }

    /// Adds a file on the local filesystem to this archive under another name.
    ///
    /// This function will only add the given path of the file specified by `path`
    /// to the archive. Any I/O error which orrcurs while opening the file
    /// or reading the appropriate metadata will be returning while using
    /// an reading the archive.  
    /// The path name for the file inside of this archive will be `name` is required
    /// to be a relative path.
    ///
    /// Note if the `path` is a directory. This will just add an entry to the archive,
    /// rather than contents of the directory.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use tar::Streamer;
    ///
    /// let mut ar = Streamer::new();
    ///
    /// // Insert the local file "foo/bar.txt" in the archive but with the name
    /// // "bar/foo.txt".
    /// ar.append_path_with_name("foo/bar.txt", "bar/foo.txt").unwrap();
    /// ```
    pub fn append_path_with_name<P: AsRef<Path>, N: AsRef<Path>>(
        &mut self,
        path: P,
        name: N,
    ) -> Result<()> {
        self.append_stream_file(path.as_ref(), Some(name.as_ref()))
    }

    /// Adds a file to this archive with the given path as the name of the file
    /// in the archive.
    ///
    /// This will use the metadata of `file` to populate a `Header`, and it will
    /// then append the file to the archive with the name `path`.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::fs::File;
    /// use tar::Streamer;
    ///
    /// let mut ar = Streamer::new();
    ///
    /// // Open the file at one location, but insert it into the archive with a
    /// // different name.
    /// let mut f = File::open("foo/bar/baz.txt").unwrap();
    /// ar.append_file("bar/baz.txt", &mut f).unwrap();
    /// ```
    pub fn append_file<P: AsRef<Path>>(&mut self, path: P, file: &mut fs::File) -> io::Result<()> {
        let stat = file.metadata()?;
        let mut header = Header::new_gnu();
        let mut encoded_header = Vec::new();
        if let Some(mut long_name_extension_entry) =
            prepare_header_path(&mut header, path.as_ref())?
        {
            encoded_header.append(&mut long_name_extension_entry);
            //self.long_name_extension_entries.insert(self.index_counter, long_name_extension_entry);
        }
        header.set_metadata_in_mode(&stat, self.mode);
        header.set_cksum();
        encoded_header.append(&mut header.as_bytes().to_vec());
        self.append_stream_data(StreamData::new_with_encoded_header(
            encoded_header,
            file.try_clone()?,
        ));
        Ok(())
    }

    /// Adds a directory to this archive with the given path as the name of the
    /// directory in the archive.
    ///
    /// Note this will not add the contents of the directory to the archive.
    /// See `append_dir_all` for recusively adding the contents of the directory.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::fs;
    /// use tar::Streamer;
    ///
    /// let mut ar = Streamer::new();
    ///
    /// // Use the directory at one location, but insert it into the archive
    /// // with a different name.
    /// ar.append_dir("bardir", ".").unwrap();
    /// ```
    pub fn append_dir<P, Q>(&mut self, path: P, src_path: Q) -> io::Result<()>
    where
        P: AsRef<Path>,
        Q: AsRef<Path>,
    {
        self.append_stream_file(src_path.as_ref(), Some(path.as_ref()))
    }

    /// Adds a directory and all of its contents (recursively) to this archive
    /// with the given path as the name of the directory in the archive.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::fs;
    /// use tar::Streamer;
    /// use std::io;
    /// let mut streamer = Streamer::new();
    ///
    /// // Use the directory at one location, but insert it into the archive
    /// // with a different name.
    /// let mut ar = Streamer::new();
    /// ar.append_dir_all("bardir", ".").unwrap();
    /// // Write the archive to the given path.
    /// let mut target_archive = fs::File::create("/home/user/my_archive.tar").unwrap();
    /// io::copy(&mut streamer, &mut target_archive).unwrap();
    /// ```
    pub fn append_dir_all<P: AsRef<Path>, S: AsRef<Path>>(
        &mut self,
        path: P,
        src_path: S,
    ) -> io::Result<()> {
        let mut stack = vec![(src_path.as_ref().to_path_buf(), true, false)];
        while let Some((src, is_dir, is_symlink)) = stack.pop() {
            let dest = path.as_ref().join(src.strip_prefix(&src_path).unwrap());
            // In case of a symlink pointing to a directory, is_dir is false, but src.is_dir() will return true
            if is_dir || (is_symlink && self.follow && src.is_dir()) {
                for entry in fs::read_dir(&src)? {
                    let entry = entry?;
                    let file_type = entry.file_type()?;
                    stack.push((entry.path(), file_type.is_dir(), file_type.is_symlink()));
                }
                if dest != Path::new("") {
                    self.append_dir(&dest, &src)?;
                }
            } else {
                #[cfg(unix)]
                {
                    let stat = fs::metadata(&src)?;
                    if !stat.is_file() {
                        self.append_special(&src)?;
                        continue;
                    }
                }
                self.append_stream_file(&src, Some(&dest))?;
            }
        }
        Ok(())
    }

    fn append_stream_data(&mut self, stream_data: StreamData) {
        self.stream_data.insert(self.index_counter, stream_data);
        self.index_counter += 1;
    }

    #[cfg(unix)]
    fn append_special(&mut self, path: &Path) -> io::Result<()> {
        prepare_special_header(path, self.mode, self.follow)?;
        self.stream_special_file.insert(
            self.index_counter,
            StreamSpecialFile::new(path, self.mode, self.follow),
        );
        self.index_counter += 1;

        Ok(())
    }

    fn append_stream_file(&mut self, path: &Path, name: Option<&Path>) -> Result<()> {
        prepare_file_header(path, name, self.mode, self.follow)?;
        let stream_file = StreamFile::new(
            path.to_path_buf(),
            name.map(|x| x.to_path_buf()),
            self.follow,
            self.mode,
        );
        self.stream_files.insert(self.index_counter, stream_file);
        self.index_counter += 1;
        Ok(())
    }
}

impl Read for Streamer {
    fn read(&mut self, buffer: &mut [u8]) -> std::result::Result<usize, std::io::Error> {
        let mut read_bytes = 0;
        'outer: loop {
            // end of archive reached, if there are remaining finish bytes, we should read them :)
            if self.streamer_metadata.current_index > self.index_counter {
                if self.streamer_metadata.finish_bytes_remaining > 0 {
                    if buffer[read_bytes..].len() > self.streamer_metadata.finish_bytes_remaining {
                        let finishing_bytes =
                            vec![0u8; self.streamer_metadata.finish_bytes_remaining];
                        self.streamer_metadata.finish_bytes_remaining -= finishing_bytes.len();
                        buffer[read_bytes..read_bytes + finishing_bytes.len()]
                            .copy_from_slice(&finishing_bytes);
                        read_bytes += finishing_bytes.len();
                    } else {
                        self.streamer_metadata.finish_bytes_remaining -= buffer[read_bytes..].len();
                        let finishing_bytes = vec![0u8; buffer[read_bytes..].len()];
                        buffer[read_bytes..read_bytes + finishing_bytes.len()]
                            .copy_from_slice(&finishing_bytes);
                        read_bytes += finishing_bytes.len();
                    }
                }
                break;
            }

            if let Some(stream_file) = self
                .stream_files
                .get_mut(&self.streamer_metadata.current_index)
            {
                //build and read the header first...
                if stream_file.cached_header_bytes.is_none() {
                    stream_file.cached_header_bytes = Some(prepare_file_header(
                        &stream_file.path,
                        stream_file.alternative_name.as_deref(),
                        stream_file.mode,
                        stream_file.follow,
                    )?)
                }
                if let Some(ref mut encoded_header) = stream_file.cached_header_bytes {
                    if encoded_header.len() > buffer[read_bytes..].len() {
                        let drained_bytes: Vec<u8> =
                            encoded_header.drain(..buffer[read_bytes..].len()).collect();
                        buffer[read_bytes..read_bytes + drained_bytes.len()]
                            .copy_from_slice(&drained_bytes);
                        read_bytes += drained_bytes.len();
                        break;
                    } else {
                        let drained_bytes: Vec<u8> = encoded_header.drain(..).collect();
                        buffer[read_bytes..read_bytes + drained_bytes.len()]
                            .copy_from_slice(&drained_bytes);
                        read_bytes += drained_bytes.len();
                    }
                }

                //...then read the appropriate data
                loop {
                    if read_bytes == buffer.len() {
                        // breaks the outer-loop to skip the update of current_index attribute, as EOF of data is not reached yet.
                        break 'outer;
                    }
                    if let Some(ref mut padding_bytes) = stream_file.padding_bytes {
                        if padding_bytes.len() > buffer[read_bytes..].len() {
                            let drained_bytes: Vec<u8> =
                                padding_bytes.drain(..buffer[read_bytes..].len()).collect();
                            buffer[read_bytes..read_bytes + drained_bytes.len()]
                                .copy_from_slice(&drained_bytes);
                            read_bytes += drained_bytes.len();
                            break 'outer;
                        } else {
                            let drained_bytes: Vec<u8> = padding_bytes.drain(..).collect();
                            buffer[read_bytes..read_bytes + drained_bytes.len()]
                                .copy_from_slice(&drained_bytes);
                            read_bytes += drained_bytes.len();
                            break;
                        }
                    } else {
                        let stat = get_stat(&stream_file.path, stream_file.follow)?;
                        if !stat.is_file() {
                            break;
                        }
                        let mut file = fs::File::open(&stream_file.path)?;
                        file.seek(SeekFrom::Start(stream_file.read_bytes as u64))?;
                        let r = file.read(&mut buffer[read_bytes..])?;
                        stream_file.read_bytes += r;
                        if r == 0 {
                            // EOF of inner data is reached, so we continue the outer-loop to skip the update of current_index as we have to
                            // read the padding bytes first, if necessary.
                            let remaining = 512 - (stream_file.read_bytes % 512);
                            if remaining < 512 {
                                stream_file.padding_bytes = Some(vec![0u8; remaining]);
                                continue 'outer;
                            } else {
                                break;
                            }
                        }
                        read_bytes += r;
                    }
                }
            }

            if let Some(stream_data) = self
                .stream_data
                .get_mut(&self.streamer_metadata.current_index)
            {
                //read the header first...
                if stream_data.encoded_header.len() > buffer[read_bytes..].len() {
                    let drained_bytes: Vec<u8> = stream_data
                        .encoded_header
                        .drain(..buffer[read_bytes..].len())
                        .collect();
                    buffer[read_bytes..read_bytes + drained_bytes.len()]
                        .copy_from_slice(&drained_bytes);
                    read_bytes += drained_bytes.len();
                    break;
                } else {
                    let drained_bytes: Vec<u8> = stream_data.encoded_header.drain(..).collect();
                    buffer[read_bytes..read_bytes + drained_bytes.len()]
                        .copy_from_slice(&drained_bytes);
                    read_bytes += drained_bytes.len();
                }

                //...then read the appropriate data
                loop {
                    if read_bytes == buffer.len() {
                        // breaks the outer-loop to skip the update of current_index attribute, as EOF of data is not reached yet.
                        break 'outer;
                    }
                    if let Some(ref mut padding_bytes) = stream_data.padding_bytes {
                        if padding_bytes.len() > buffer[read_bytes..].len() {
                            let drained_bytes: Vec<u8> =
                                padding_bytes.drain(..buffer[read_bytes..].len()).collect();
                            buffer[read_bytes..read_bytes + drained_bytes.len()]
                                .copy_from_slice(&drained_bytes);
                            read_bytes += drained_bytes.len();
                            break 'outer;
                        } else {
                            let drained_bytes: Vec<u8> = padding_bytes.drain(..).collect();
                            buffer[read_bytes..read_bytes + drained_bytes.len()]
                                .copy_from_slice(&drained_bytes);
                            read_bytes += drained_bytes.len();
                            break;
                        }
                    } else {
                        let r = stream_data.data.read(&mut buffer[read_bytes..])?;
                        stream_data.read_bytes += r;
                        if r == 0 {
                            // EOF of inner data is reached, so we continue the outer-loop to skip the update of current_index as we have to
                            // read the padding bytes first, if necessary.
                            let remaining = 512 - (stream_data.read_bytes % 512);
                            if remaining < 512 {
                                stream_data.padding_bytes = Some(vec![0u8; remaining]);
                                continue 'outer;
                            } else {
                                break;
                            }
                        }
                        read_bytes += r;
                    }
                }
            }

            if let Some(stream_special_file) = self
                .stream_special_file
                .get_mut(&self.streamer_metadata.current_index)
            {
                //Zero padding should not necessary here.
                if stream_special_file.cached_header_bytes.is_none() {
                    stream_special_file.cached_header_bytes = Some(prepare_special_header(
                        &stream_special_file.path,
                        stream_special_file.mode,
                        stream_special_file.follow,
                    )?);
                }
                if let Some(ref mut encoded_header) = stream_special_file.cached_header_bytes {
                    if encoded_header.len() > buffer[read_bytes..].len() {
                        let drained_bytes: Vec<u8> =
                            encoded_header.drain(..buffer[read_bytes..].len()).collect();
                        buffer[read_bytes..read_bytes + drained_bytes.len()]
                            .copy_from_slice(&drained_bytes);
                        read_bytes += drained_bytes.len();
                        break;
                    } else {
                        let drained_bytes: Vec<u8> = encoded_header.drain(..).collect();
                        buffer[read_bytes..read_bytes + drained_bytes.len()]
                            .copy_from_slice(&drained_bytes);
                        read_bytes += drained_bytes.len();
                    }
                }
            }

            if let Some(stream_link) = self
                .stream_link
                .get_mut(&self.streamer_metadata.current_index)
            {
                //Zero padding should not necessary here.
                if stream_link.encoded_header.len() > buffer[read_bytes..].len() {
                    let drained_bytes: Vec<u8> = stream_link
                        .encoded_header
                        .drain(..buffer[read_bytes..].len())
                        .collect();
                    buffer[read_bytes..read_bytes + drained_bytes.len()]
                        .copy_from_slice(&drained_bytes);
                    read_bytes += drained_bytes.len();
                    break;
                } else {
                    let drained_bytes: Vec<u8> = stream_link.encoded_header.drain(..).collect();
                    buffer[read_bytes..read_bytes + drained_bytes.len()]
                        .copy_from_slice(&drained_bytes);
                    read_bytes += drained_bytes.len();
                }
            }
            self.streamer_metadata.current_index += 1;
        }
        self.streamer_metadata.read_bytes += read_bytes;
        Ok(read_bytes)
    }
}

fn prepare_file_header(
    path: &Path,
    name: Option<&Path>,
    mode: HeaderMode,
    follow: bool,
) -> io::Result<Vec<u8>> {
    let stat = get_stat(path, follow)?;
    let ar_name = name.unwrap_or(path);

    //generate and prepare appropriate header
    let mut encoded_header = Vec::new();
    let mut header = Header::new_gnu();

    if let Some(mut long_name_extension_entry) = prepare_header_path(&mut header, ar_name)? {
        encoded_header.append(&mut long_name_extension_entry);
    }
    header.set_metadata_in_mode(&stat, mode);
    if stat.file_type().is_symlink() {
        let link_name = fs::read_link(path)?;
        if let Some(mut long_name_extension_entry) = prepare_header_link(&mut header, &link_name)? {
            encoded_header.append(&mut long_name_extension_entry);
        }
    }
    header.set_cksum();
    encoded_header.append(&mut header.as_bytes().to_vec());
    Ok(encoded_header)
}

fn get_stat<P: AsRef<Path>>(path: P, follow: bool) -> io::Result<fs::Metadata> {
    if follow {
        fs::metadata(path.as_ref()).map_err(|err| {
            io::Error::new(
                err.kind(),
                format!(
                    "{} when getting metadata for {}",
                    err,
                    path.as_ref().display()
                ),
            )
        })
    } else {
        fs::symlink_metadata(path.as_ref()).map_err(|err| {
            io::Error::new(
                err.kind(),
                format!(
                    "{} when getting metadata for {}",
                    err,
                    path.as_ref().display()
                ),
            )
        })
    }
}

#[cfg(unix)]
fn prepare_special_header(path: &Path, mode: HeaderMode, follow: bool) -> io::Result<Vec<u8>> {
    let stat = get_stat(path, follow)?;

    let file_type = stat.file_type();
    let entry_type;
    if file_type.is_socket() {
        // sockets can't be archived
        return Err(other(&format!(
            "{}: socket can not be archived",
            path.display()
        )));
    } else if file_type.is_fifo() {
        entry_type = EntryType::Fifo;
    } else if file_type.is_char_device() {
        entry_type = EntryType::Char;
    } else if file_type.is_block_device() {
        entry_type = EntryType::Block;
    } else {
        return Err(other(&format!("{} has unknown file type", path.display())));
    }

    let mut encoded_header = Vec::new();
    let mut header = Header::new_gnu();
    header.set_metadata_in_mode(&stat, mode);
    if let Some(mut long_name_extension_entry) = prepare_header_path(&mut header, path)? {
        encoded_header.append(&mut long_name_extension_entry);
    }
    header.set_entry_type(entry_type);
    let dev_id = stat.rdev();
    let dev_major = ((dev_id >> 32) & 0xffff_f000) | ((dev_id >> 8) & 0x0000_0fff);
    let dev_minor = ((dev_id >> 12) & 0xffff_ff00) | ((dev_id) & 0x0000_00ff);
    header.set_device_major(dev_major as u32)?;
    header.set_device_minor(dev_minor as u32)?;

    header.set_cksum();
    encoded_header.append(&mut header.as_bytes().to_vec());

    Ok(encoded_header)
}

// function tries to encode the path directly in header.
// Returns an Ok(None) if everything is fine.
// Returns an Ok(Some(StreamData)) as an extra entry to emit the "long file name".
fn prepare_header_path(header: &mut Header, path: &Path) -> Result<Option<Vec<u8>>> {
    // Try to encode the path directly in the header, but if it ends up not
    // working (probably because it's too long) then try to use the GNU-specific
    // long name extension by emitting an entry which indicates that it's the
    // filename.
    let mut extra_entry = None;
    if let Err(e) = header.set_path(path) {
        let data = path2bytes(path)?;
        let max = header.as_old().name.len();
        // Since `e` isn't specific enough to let us know the path is indeed too
        // long, verify it first before using the extension.
        if data.len() < max {
            return Err(e);
        }
        let header2 = prepare_header(data.len() as u64, b'L');
        // null-terminated string
        let mut data2 = data.to_vec();
        data2.push(0);
        //pad zeros if necessary
        let remaining = 512 - (data2.len() % 512);
        if remaining < 512 {
            data2.append(&mut vec![0u8; remaining]);
        }
        let mut entry_data = header2.as_bytes().to_vec();
        entry_data.append(&mut data2);
        extra_entry = Some(entry_data);

        // Truncate the path to store in the header we're about to emit to
        // ensure we've got something at least mentioned. Note that we use
        // `str`-encoding to be compatible with Windows, but in general the
        // entry in the header itself shouldn't matter too much since extraction
        // doesn't look at it.
        let truncated = match str::from_utf8(&data[..max]) {
            Ok(s) => s,
            Err(e) => str::from_utf8(&data[..e.valid_up_to()]).unwrap(),
        };
        header.set_path(truncated)?;
    }
    Ok(extra_entry)
}

fn prepare_header_link(header: &mut Header, link_name: &Path) -> Result<Option<Vec<u8>>> {
    // Same as previous function but for linkname
    let mut extra_entry = None;
    if let Err(e) = header.set_link_name(link_name) {
        let data = path2bytes(link_name)?;
        if data.len() < header.as_old().linkname.len() {
            return Err(e);
        }
        let header2 = prepare_header(data.len() as u64, b'K');
        // null-terminated string
        let mut data2 = data.to_vec();
        data2.push(0);
        //pad zeros if necessary
        let remaining = 512 - (data2.len() % 512);
        if remaining < 512 {
            data2.append(&mut vec![0u8; remaining]);
        }
        let mut entry_data = header2.as_bytes().to_vec();
        entry_data.append(&mut data2);
        extra_entry = Some(entry_data);
    }
    Ok(extra_entry)
}

#[cfg(any(windows, target_arch = "wasm32"))]
fn path2bytes(p: &Path) -> std::io::Result<&[u8]> {
    p.as_os_str()
        .to_str()
        .map(|s| s.as_bytes())
        .ok_or_else(|| other(&format!("path {} was not valid Unicode", p.display())))
        .map(|bytes| {
            if bytes.contains(&b'\\') {
                // Normalize to Unix-style path separators
                let mut bytes = bytes.to_owned();
                for b in &mut bytes {
                    if *b == b'\\' {
                        *b = b'/';
                    }
                }
                bytes
            } else {
                bytes.to_vec()
            }
        })
}

#[cfg(unix)]
fn path2bytes(p: &Path) -> std::io::Result<&[u8]> {
    Ok(p.as_os_str().as_bytes())
}
