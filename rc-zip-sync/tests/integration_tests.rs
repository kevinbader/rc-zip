use rc_zip::{
    corpus::{self, zips_dir, Case, Files},
    error::Error,
    parse::Archive,
};
use rc_zip_sync::{ArchiveHandle, HasCursor, ReadZip, ReadZipStreaming};

use std::{fs::File, io::Read};

fn check_case<F: HasCursor>(test: &Case, archive: Result<ArchiveHandle<'_, F>, Error>) {
    corpus::check_case(test, archive.as_ref().map(|ar| -> &Archive { ar }));
    let archive = match archive {
        Ok(archive) => archive,
        Err(_) => return,
    };

    if let Files::ExhaustiveList(files) = &test.files {
        for file in files {
            tracing::info!("checking file {}", file.name);
            let entry = archive
                .by_name(file.name)
                .unwrap_or_else(|| panic!("entry {} should exist", file.name));

            tracing::info!("got entry for {}", file.name);
            corpus::check_file_against(file, &entry, &entry.bytes().unwrap()[..])
        }
    }
}

#[test_log::test]
fn read_from_slice() {
    let bytes = std::fs::read(zips_dir().join("test.zip")).unwrap();
    let slice = &bytes[..];
    let archive = slice.read_zip().unwrap();
    assert_eq!(archive.entries().count(), 2);
}

#[test_log::test]
fn read_from_file() {
    let f = File::open(zips_dir().join("test.zip")).unwrap();
    let archive = f.read_zip().unwrap();
    assert_eq!(archive.entries().count(), 2);
}

#[test_log::test]
fn real_world_files() {
    for case in corpus::test_cases() {
        tracing::info!("============ testing {}", case.name);

        let guarded_path = case.absolute_path();
        let file = File::open(&guarded_path.path).unwrap();
        let archive = file.read_zip().map_err(Error::from);
        check_case(&case, archive);
        drop(guarded_path)
    }
}

#[test_log::test]
fn streaming() {
    for case in corpus::streaming_test_cases() {
        let guarded_path = case.absolute_path();
        let file = File::open(&guarded_path.path).unwrap();

        let mut entry = file
            .stream_zip_entries_throwing_caution_to_the_wind()
            .unwrap();
        loop {
            let mut v = vec![];
            let n = entry.read_to_end(&mut v).unwrap();
            tracing::trace!("entry {} read {} bytes", entry.entry().name, n);

            match entry.finish().unwrap() {
                Some(next) => entry = next,
                None => break,
            }
        }

        drop(guarded_path)
    }
}
