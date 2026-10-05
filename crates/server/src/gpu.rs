//! The render node the host composites and converts on. The nested
//! compositor is steered to the same node through linux-dmabuf feedback.

use std::fs::File;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use anyhow::Context;
use smithay::backend::allocator::Format;
use smithay::backend::allocator::gbm::GbmDevice;
use smithay::backend::egl::{EGLContext, EGLDisplay};
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::wayland::dmabuf::{DmabufFeedback, DmabufFeedbackBuilder};

pub struct Gpu {
    pub renderer: GlesRenderer,
    pub feedback: DmabufFeedback,
}

pub fn open(node: &Path) -> anyhow::Result<Gpu> {
    let file = File::options()
        .read(true)
        .write(true)
        .open(node)
        .with_context(|| format!("opening {}", node.display()))?;
    let dev = file.metadata()?.rdev();
    let gbm = GbmDevice::new(file).context("gbm device")?;
    // SAFETY: the GBM device outlives the display (EGLDisplay keeps it alive).
    let display = unsafe { EGLDisplay::new(gbm) }.context("EGL display")?;
    let context = EGLContext::new(&display).context("EGL context")?;
    // SAFETY: the context is fresh and used only by this renderer.
    let renderer = unsafe { GlesRenderer::new(context) }.context("GLES renderer")?;

    let texture_formats: Vec<Format> =
        renderer.egl_context().dmabuf_texture_formats().iter().copied().collect();
    tracing::info!(
        node = %node.display(),
        texture_formats = texture_formats.len(),
        render_formats = renderer.egl_context().dmabuf_render_formats().iter().count(),
        "GPU ready"
    );
    let feedback = DmabufFeedbackBuilder::new(dev, texture_formats)
        .build()
        .context("dmabuf feedback")?;
    Ok(Gpu { renderer, feedback })
}
