//! Shared-memory capture buffers (`wl_shm`), used by both capture backends.

use std::io::{Read as _, Seek as _, SeekFrom};
use std::os::fd::AsFd as _;

use anyhow::{bail, Context as _};
use wayland_client::protocol::{
    wl_buffer::WlBuffer,
    wl_shm::{self, WlShm},
    wl_shm_pool::WlShmPool,
};
use wayland_client::QueueHandle;

use crate::capture::State;

/// A pixel buffer backed by an anonymous temp file and shared with the
/// compositor through `wl_shm`.
///
/// The compositor writes the captured frame into it; `read_pixels` copies the
/// bytes back out for decoding.
pub struct ShmBuffer {
    file: std::fs::File,
    /// Kept so the pool object clearly outlives the buffer created from it.
    _pool: WlShmPool,
    buffer: WlBuffer,
}

impl ShmBuffer {
    /// Allocate and register a `width` x `height` RGBA buffer with `stride`
    /// bytes per row.
    pub fn create(
        shm: &WlShm,
        qh: &QueueHandle<State>,
        width: u32,
        height: u32,
        stride: u32,
        format: wl_shm::Format,
    ) -> anyhow::Result<Self> {
        if width == 0 || height == 0 {
            bail!("refusing to allocate a {width}x{height} capture buffer");
        }
        if (stride as u64) < u64::from(width) * 4 {
            bail!("stride {stride} is too small for {width} RGBA pixels");
        }
        let size = u64::from(stride)
            .checked_mul(u64::from(height))
            .context("capture buffer size overflows")?;
        if size > i32::MAX as u64 {
            bail!("capture buffer of {size} bytes exceeds the wl_shm pool limit");
        }

        let file =
            tempfile::tempfile().context("failed to allocate an anonymous capture buffer")?;
        file.set_len(size)
            .context("failed to size the capture buffer")?;

        let pool = shm.create_pool(file.as_fd(), size as i32, qh, ());
        let buffer = pool.create_buffer(
            0,
            width as i32,
            height as i32,
            stride as i32,
            format,
            qh,
            (),
        );
        Ok(Self {
            file,
            _pool: pool,
            buffer,
        })
    }

    /// The protocol object to hand to `attach_buffer` (ext) or `copy` (wlr).
    pub fn buffer(&self) -> &WlBuffer {
        &self.buffer
    }

    /// Copy the compositor-written pixels out of the shared buffer.
    pub fn read_pixels(&mut self, stride: u32, height: u32) -> anyhow::Result<Vec<u8>> {
        let size = u64::from(stride) * u64::from(height);
        let mut data = vec![0u8; size as usize];
        self.file
            .seek(SeekFrom::Start(0))
            .context("failed to rewind the capture buffer")?;
        self.file
            .read_exact(&mut data)
            .with_context(|| format!("failed to read {size} bytes back from the capture buffer"))?;
        Ok(data)
    }
}

impl Drop for ShmBuffer {
    /// Release the compositor-side objects.
    ///
    /// wayland-client does not destroy a proxy when it is dropped. Without
    /// these requests the compositor keeps every pool mapped for the life of
    /// the connection: one full frame of memory leaked per capture (#49).
    fn drop(&mut self) {
        self.buffer.destroy();
        self._pool.destroy();
    }
}
