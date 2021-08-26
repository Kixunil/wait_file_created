//! Robust implementation of waiting for file to be created.
//!
//! If your application has to wait for another application to create a file at certain path it's
//! not entirely obvious how to do it without race conditions. This crate helps with that and
//! provides a very simple API. See the shorthand functions provided in this crate first - you
//! likely only need one of them.
//!
//! It uses `inotify` which is available in Linux only to wait for the file. `notify` crate was
//! specifically not used to ensure high robustness. PRs to add other platforms will be accepted if
//! I can not see race conditions or other bugs in them.
//!
//! ## Example
//!
//! ```no_run
//!     use std::io::Read;
//!
//!     let mut file = wait_file_created::robust_wait_read("my/expected/file").unwrap();
//!     let mut contents = String::new();
//!     file.read_to_string(&mut contents).unwrap();
//! ```
//!
//! As you can see the function returns an already-opened file which minimizes risk of race
//! conditions. Unfortunately it can not be entirely elliminated in all scenarios.
//!
//! ## Limitations
//!
//! This library can **not** *guarantee* that the file opened was written completely.
//! Specifically, if the application writing it has created it before your application attempted to
//! open it but is still writing after that time then your application will observe incomplete
//! data.
//!
//! You must ensure that your application can handle incomplete data or (much better) ensure that
//! the application creating the file does so *atomically* - that is create a temporary file first,
//! write to it and then move it over to the final destination. The library is specifically
//! designed to handle this scenario so you may rely on that.
//!
//! Note that in Linux there is another mechanism for atomically creating files.
//! A file can be opened using `O_TMPFILE` which creates an anonymous file.
//! After populating it the file can be linked to the directory using `linkat()` syscall.
//!
//! If an application is using *`O_TMPFILE` method* the notfication will only be received after the
//! file descriptor is *closed* even though it would be safe to open it after it was *created*.
//! The `assume_create_is_atomic()` method can be used to indicate that and request the file to be
//! opened right away. This may improve performance or in case the application wants to keep the
//! file descriptor opened it ensures the code functions at all. The same method can be used if the
//! file is supposed to be empty.

use std::fs::{File, OpenOptions};
use std::path::Path;
use std::io;
use std::time::Duration;

/// Builder allowing configuration beyond what shorthand functions enable.
///
/// In simple scenarios you only need shorthand functions at the top-level of this crate.
/// However sometimes it's needed to configure the behavior more precisely.
/// These options enable you to fine-tune the settings to match your needs.
///
/// Pay attention to how default values of this builder differ from those in shorthand functions!
/// Also make sure you understand the implications of the settings.
pub struct Options {
    open_options: OpenOptions,
    retry_flukes: bool,
    create_is_atomic: bool,
    polling_fallback: Option<Duration>,
}

impl Options {
    /// Creates the builder giving it initial configuration for opening files.
    ///
    /// This crates configuration with **no** robustness settings by default.
    /// That means `retry_on_fluke` is `false` and there is no polling fallback.
    /// *Creation* of the file is **not** assumed to be atomic.
    /// You must add them explicitly using the builder methods!
    pub fn with_open_options(open_options: OpenOptions) -> Self {
        Options {
            open_options,
            retry_flukes: false,
            create_is_atomic: false,
            polling_fallback: None,
        }
    }

    /// Tells what to do if the file was deleted between notification was received and file opened.
    ///
    /// It can happen in theory that an application creates file, writes to it closes it and then
    /// deletes it. In this case the application may not be able to open the file in time.
    /// If `retry_on_fluke` is set to `true` waiting will continue until another file is created
    /// and opened.
    pub fn retry_on_fluke(mut self, retry: bool) -> Self {
        self.retry_flukes = retry;
        self
    }

    /// Fallback to polling if inotify calls fail for any reason.
    ///
    /// If any inotify syscall fails it could be that the file may still be opened.
    /// This will always be attempted but it will only be retried again after a delay if this
    /// interval is set.
    ///
    /// Note that by default shorthand functions in this library use 2 second interval.
    pub fn polling_fallback_interval(mut self, interval: Duration) -> Self {
        self.polling_fallback = Some(interval);
        self
    }

    /// Indicates that file creation is atomic and you want the file to be opened right away.
    ///
    /// Some applications may create a file atomically and then keep the file descriptor around.
    /// This would cause the notification to not be received even though it would be safe to
    /// continue. If you know for a fact that the application creating the file uses atomic
    /// creation you should indicate so using this method so that an earlier notification is used
    /// to determine when to open the file.
    ///
    /// You can learm more about this in the `Limitations` section of the crate documentation
    pub fn assume_create_is_atomic(mut self, is_atomic: bool) -> Self {
        self.create_is_atomic = is_atomic;
        self
    }

    /// Opens the file once it's available waiting for it to be created if it doesn't exist yet.
    ///
    /// This method uses settings specified in the builder to wait for file to be created and open
    /// it.
    #[inline]
    pub fn open_when_created<P: AsRef<Path>>(&self, path: P) -> io::Result<File> {
        // Monomorphise the method to avoid machine code blowup
        self.internal_open_when_created(path.as_ref())
    }

    fn internal_open_when_created(&self, path: &Path) -> io::Result<File> {
        use inotify::WatchMask;

        match inotify::Inotify::init() {
            Ok(mut inotify) => {
                let mut mask = WatchMask::CLOSE_WRITE | WatchMask::MOVED_TO | WatchMask::DELETE_SELF | WatchMask::ONLYDIR;
                if self.create_is_atomic {
                    mask |= WatchMask::CREATE;
                }

                match inotify.add_watch(path, mask) {
                    Ok(_) => (),
                    Err(error) => return self.try_fallback_open(path, error),
                };

                self.wait_for_file(inotify, path)

            },
            Err(error) => self.try_fallback_open(path, error),
        }
    }

    fn try_fallback_open(&self, path: &Path, inotify_error: io::Error) -> io::Result<File> {
        loop {
            match self.open_options.open(path) {
                Ok(file) => return Ok(file),
                Err(error) if error.kind() == io::ErrorKind::NotFound => (),
                Err(error) => return Err(error),
            }

            match &self.polling_fallback {
                Some(interval) => std::thread::sleep(*interval),
                None => return Err(inotify_error),
            }
        }
    }

    fn wait_for_file(&self, mut inotify: inotify::Inotify, path: &Path) -> io::Result<File> {
        use inotify::EventMask;

        let mut buffer = [0; 4096];
        let mut not_found_is_ok = true;

        loop {
            match self.open_options.open(path) {
                Ok(file) => return Ok(file),
                Err(error) if error.kind() == io::ErrorKind::NotFound && not_found_is_ok => (),
                Err(error) => return Err(error),
            }

            #[cfg(all(test, test_delay_after_check))]
            {
                // We want to make sure we DO receive notification after we checked existence of
                // the file even though we didn't ACTIVELY wait for it yet.
                std::thread::sleep(std::time::Duration::from_secs(7));
            }

            let mut found = false;
            while !found {
                let events = match inotify.read_events_blocking(&mut buffer) {
                    Ok(events) => events,
                    Err(error) => return self.try_fallback_open(path, error),
                };

                for event in events {
                    if event.mask.contains(EventMask::IGNORED) {
                        return self.try_fallback_open(path, io::Error::from(io::ErrorKind::NotFound));
                    }
                    if event.name == Some(path.as_os_str()) {
                        found = true;
                    }
                }
            }

            not_found_is_ok = self.retry_flukes;
        }
    }
}

/// Wait for file being available and open it for reading once it is falling back on some errors.
///
/// If `inotify` is unavailable this will poll every 2 seconds.
///
/// This is a shorthand for creating `Options`, setting `retry_on_fluke` to `true` and
/// `polling_fallback_interval` to two seconds then calling `open_when_created`.
pub fn robust_wait_read<P: AsRef<Path>>(path: P) -> io::Result<File> {
    let mut open_options = OpenOptions::new();
    open_options.read(true);

    Options::with_open_options(open_options)
        .retry_on_fluke(true)
        .polling_fallback_interval(Duration::from_secs(2))
        .open_when_created(path)
}

/// Wait for file being available and open it for reading and writing once it is falling back on some errors.
///
/// If `inotify` is unavailable this will poll every 2 seconds.
///
/// This is a shorthand for creating `Options`, setting `retry_on_fluke` to `true` and
/// `polling_fallback_interval` to two seconds then calling `open_when_created`.
pub fn robust_wait_read_write<P: AsRef<Path>>(path: P) -> io::Result<File> {
    let mut open_options = OpenOptions::new();
    open_options.read(true).write(true);

    Options::with_open_options(open_options)
        .retry_on_fluke(true)
        .polling_fallback_interval(Duration::from_secs(2))
        .open_when_created(path)
}

/// Wait for file being available and open it for reading and appending once it is falling back on some errors.
///
/// If `inotify` is unavailable this will poll every 2 seconds.
///
/// This is a shorthand for creating `Options`, setting `retry_on_fluke` to `true` and
/// `polling_fallback_interval` to two seconds then calling `open_when_created`.
pub fn robust_wait_read_append<P: AsRef<Path>>(path: P) -> io::Result<File> {
    let mut open_options = OpenOptions::new();
    open_options.read(true).append(true);

    Options::with_open_options(open_options)
        .retry_on_fluke(true)
        .polling_fallback_interval(Duration::from_secs(2))
        .open_when_created(path)
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_wait() {
        use std::io::{Read, Write};

        let test_string = "satoshi nakamoto";
        let temp_dir = mktemp::Temp::new_dir().unwrap();
        let file_path = temp_dir.join("test");
        let file_path_thread = file_path.clone();
        let thread = std::thread::spawn(move || {
            // Sleeps between operations should make it easier to detect inconsistencies
            std::thread::sleep(std::time::Duration::from_secs(2));
            let mut file = std::fs::File::create(&file_path_thread).unwrap();
            std::thread::sleep(std::time::Duration::from_secs(1));
            file.write_all(test_string.as_bytes()).unwrap();
            std::thread::sleep(std::time::Duration::from_secs(1));
        });
        let mut file = super::robust_wait_read(&file_path).unwrap();
        let mut contents = String::new();
        file.read_to_string(&mut contents).unwrap();
        assert_eq!(contents, test_string);
        thread.join().unwrap();
    }
}
