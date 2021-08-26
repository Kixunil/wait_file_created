# Wait for file to be created

Rust crate implementing robust waiting for file to be created.

## About

If your application has to wait for another application to create a file at certain path it's
not entirely obvious how to do it without race conditions. This crate helps with that and
provides a very simple API. See the shorthand functions provided in this crate first - you
likely only need one of them.

It uses `inotify` which is available in Linux only to wait for the file. `notify` crate was
specifically not used to ensure high robustness. PRs to add other platforms will be accepted if I
can not see race conditions or other bugs in them.

## Example

```rust
    use std::io::Read;

    let mut file = wait_file_created::robust_wait_read("my/expected/file").unwrap();
    let mut contents = String::new();
    file.read_to_string(&mut contents).unwrap();
```

As you can see the function returns an already-opened file which minimizes risk of race
conditions. Unfortunately it can not be entirely elliminated in all scenarios.

## Limitations

This library can **not** *guarantee* that the file opened was written completely.
Specifically, if the application writing it has created it before your application attempted to
open it but is still writing after that time then your application will observe incomplete
data.

You must ensure that your application can handle incomplete data or (much better) ensure that
the application creating the file does so *atomically* - that is create a temporary file first,
write to it and then move it over to the final destination. The library is specifically
designed to handle this scenario so you may rely on that.

Note that in Linux there is another mechanism for atomically creating files.
A file can be opened using `O_TMPFILE` which creates an anonymous file.
After populating it it can be linked to the directory using `linkat()` syscall.

If an application is using *this method* the notfication will only be received after the
file descriptor is *closed* even though it would be safe to open it after it was *created*.
The `assume_create_is_atomic()` method can be used to indicate that and request the file to be
opened right away. This may improve performance or in case the application wants to keep the
file descriptor opened it ensures the code functions at all. The same method can be used if the
file is supposed to be empty.

## License

MITNFA
