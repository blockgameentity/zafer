//! Presenting a rendered image through the swapchain.
//!
//! `vkQueuePresentKHR` returns three results that are not failures: the
//! image was presented on a surface that no longer matches the swapchain
//! (`SUBOPTIMAL_KHR`), or the queue is full and nothing was presented
//! (`NOT_READY` on a `MAILBOX`/`IMMEDIATE` swapchain). A caller must
//! distinguish them from a real error, and the console's titles do not care
//! which happened — they render the next frame either way. [`Outcome`] is
//! that distinction, decided in a pure function so it is testable without a
//! driver.

use ash::vk;

/// What a present attempt did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The image is on screen and the swapchain still matches the surface.
    Presented,
    /// The image is on screen but the swapchain is stale.
    //
    /// The next frame must recreate it; the frame itself was not lost, so a
    /// caller should not re-present.
    Suboptimal,
    /// The queue was full and nothing was presented.
    ///
    /// The caller should present the same image again rather than acquire a
    /// new one, or the frame is dropped.
    NotReady,
}

impl Outcome {
    /// Classifies a `vkQueuePresentKHR` result.
    ///
    /// `Ok(())` is [`Outcome::Presented`] and the two "not really errors"
    /// codes are their own outcomes; everything else is a failure and is
    /// passed through as the driver's message.
    pub fn of(result: vk::Result) -> Result<Outcome, String> {
        match result {
            vk::Result::SUCCESS => Ok(Outcome::Presented),
            vk::Result::SUBOPTIMAL_KHR => Ok(Outcome::Suboptimal),
            vk::Result::ERROR_OUT_OF_DATE_KHR => Ok(Outcome::Suboptimal),
            vk::Result::NOT_READY => Ok(Outcome::NotReady),
            other => Err(format!("vkQueuePresentKHR: {other:?}")),
        }
    }

    /// Whether the swapchain must be recreated before the next frame.
    pub fn recreate(self) -> bool {
        matches!(self, Outcome::Suboptimal)
    }

    /// Whether the same image must be presented again.
    pub fn retry(self) -> bool {
        matches!(self, Outcome::NotReady)
    }
}

/// Classifies the result of `vkAcquireNextImageKHR`.
///
/// The same two harmless codes appear there, and `NOT_READY` is normal on a
/// swapchain whose images are all held by the compositor: the caller waits
/// on the semaphore it passed and tries again, so this says so instead of
/// failing.
pub fn acquired(result: vk::Result) -> Result<Option<u32>, String> {
    match result {
        vk::Result::SUCCESS => Ok(None),
        vk::Result::NOT_READY | vk::Result::TIMEOUT => Ok(None),
        vk::Result::SUBOPTIMAL_KHR | vk::Result::ERROR_OUT_OF_DATE_KHR => Ok(None),
        other => Err(format!("vkAcquireNextImageKHR: {other:?}")),
    }
}
