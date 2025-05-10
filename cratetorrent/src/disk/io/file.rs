use std::{
    fs::OpenOptions,
    io::{IoSlice, IoSliceMut},
    path::Path,
};

use nix::sys::uio::{preadv, pwritev};

use crate::{
    disk::error::*,
    iovecs,
    iovecs::IoVec,
    storage_info::FileSlice,
    FileInfo,
};

pub(crate) struct TorrentFile {
    pub info: FileInfo,
    pub handle: std::fs::File,
}

impl TorrentFile {
    pub fn new(
        download_dir: &Path,
        info: FileInfo,
    ) -> Result<Self, NewTorrentError> {
        let path = download_dir.join(&info.path);
        let handle = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(NewTorrentError::Io)?;
        Ok(Self { info, handle })
    }

    pub fn write<'a>(
        &self,
        file_slice: FileSlice,
        blocks: &'a mut [IoVec<&'a [u8]>],
    ) -> Result<&'a mut [IoVec<&'a [u8]>], WriteError> {
        let mut iovecs = iovecs::IoVecs::bounded(blocks, file_slice.len as usize);

        let mut total_written = 0;
        while !iovecs.as_slice().is_empty() {
            let ios: Vec<IoSlice> = iovecs
                .as_slice()
                .iter()
                .map(|iov| IoSlice::new(iov.as_slice()))
                .collect();

            let n = pwritev(&self.handle, &ios, file_slice.offset as i64)
                .map_err(|e| WriteError::Io(e.into()))?;

            total_written += n;
            if total_written as u64 == file_slice.len {
                break;
            }

            iovecs.advance(n);
        }

        Ok(iovecs.into_tail())
    }

    pub fn read<'a>(
        &self,
        file_slice: FileSlice,
        io_vecs: &'a mut [IoVec<&'a mut [u8]>],
    ) -> Result<&'a mut [IoVec<&'a mut [u8]>], ReadError> {
        let mut bufs = io_vecs;
        let mut total_read = 0;

        while !bufs.is_empty() && (total_read as u64) < file_slice.len {
            // Build IoSliceMut directly from each &mut [u8]
            let mut ios: Vec<IoSliceMut> = bufs
                .iter_mut()
                .map(|iov| IoSliceMut::new(iov.as_mut_slice()))
                .collect();

            let n = preadv(
                &self.handle,
                &mut ios,
                file_slice.offset as i64 + total_read as i64,
            )
                .map_err(|e| ReadError::Io(e.into()))?;

            if n == 0 {
                return Err(ReadError::MissingData);
            }

            total_read += n;
            bufs = iovecs::advance(bufs, n);
        }

        Ok(bufs)
    }
}
