use std::{error::Error, io};

const USAGE: &str = "Usage: vtt stat_long_filename <count>";
#[cfg(target_os = "linux")]
const STATX_BASIC_STATS: u32 = 0x07ff;

pub fn run(args: &[String]) -> Result<(), Box<dyn Error>> {
    let count = parse_count(args)?;
    access_generated_path(count, metadata)?;
    Ok(())
}

fn parse_count(args: &[String]) -> Result<usize, String> {
    let [count] = args else { return Err(USAGE.to_owned()) };
    count.parse().map_err(|_| USAGE.to_owned())
}

fn generated_path(count: usize) -> String {
    "x".repeat(count)
}

fn access_generated_path(
    count: usize,
    mut metadata: impl FnMut(&str) -> io::Result<()>,
) -> io::Result<()> {
    let path = generated_path(count);
    match metadata(&path) {
        Ok(()) => Ok(()),
        Err(error)
            if error.kind() == io::ErrorKind::NotFound
                || error.raw_os_error() == Some(libc::ENAMETOOLONG) =>
        {
            Ok(())
        }
        Err(error) => Err(error),
    }
}

#[cfg(target_os = "linux")]
fn metadata(path: &str) -> io::Result<()> {
    use std::{ffi::CString, mem::MaybeUninit};

    let path = CString::new(path)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
    // Linux's `struct statx` is 256 bytes and 64-bit aligned on supported targets.
    let mut statx = MaybeUninit::<[u64; 32]>::uninit();
    // SAFETY: `path` is NUL-terminated, `statx` points to writable storage, and
    // all six variadic arguments use the type read by fspy's syscall interceptor.
    let result = unsafe {
        libc::syscall(
            libc::SYS_statx,
            libc::c_long::from(libc::AT_FDCWD),
            path.as_ptr() as libc::c_long,
            0 as libc::c_long,
            libc::c_long::from(STATX_BASIC_STATS),
            statx.as_mut_ptr() as libc::c_long,
            0 as libc::c_long,
        )
    };
    if result == -1 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

#[cfg(not(target_os = "linux"))]
fn metadata(path: &str) -> io::Result<()> {
    std::fs::metadata(path).map(|_| ())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_owned()).collect()
    }

    #[test]
    fn parses_exactly_one_usize_count() {
        assert_eq!(parse_count(&strings(&["1048576"])).unwrap(), 1_048_576);
        assert_eq!(parse_count(&[]).unwrap_err(), USAGE);
        assert_eq!(parse_count(&strings(&["1", "2"])).unwrap_err(), USAGE);
        assert_eq!(parse_count(&strings(&["many"])).unwrap_err(), USAGE);
    }

    #[test]
    fn generates_an_exact_relative_non_nul_ascii_path() {
        let path = generated_path(4096);

        assert_eq!(path.len(), 4096);
        assert!(Path::new(&path).is_relative());
        assert!(path.is_ascii());
        assert!(!path.as_bytes().contains(&0));
    }

    #[test]
    fn makes_one_metadata_access_and_accepts_enametoolong() {
        let mut calls = 0;

        access_generated_path(4096, |path| {
            calls += 1;
            assert_eq!(path.len(), 4096);
            Err(io::Error::from_raw_os_error(libc::ENAMETOOLONG))
        })
        .unwrap();

        assert_eq!(calls, 1);
    }

    #[test]
    fn accepts_a_missing_path() {
        access_generated_path(8, |_| Err(io::Error::from(io::ErrorKind::NotFound))).unwrap();
    }

    #[test]
    fn propagates_unexpected_metadata_errors() {
        let error =
            access_generated_path(8, |_| Err(io::Error::from(io::ErrorKind::PermissionDenied)))
                .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }
}
