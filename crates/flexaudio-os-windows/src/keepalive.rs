//! Silent shared-mode rendering keeps classic loopback device-clocked at idle.

use flexaudio_core::types::{Error, Result};
use windows::core::PCWSTR;
use windows::Win32::Media::Audio::{
    IAudioClient, IAudioRenderClient, IMMDevice, AUDCLNT_BUFFERFLAGS_SILENT,
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, WAVEFORMATEX,
};
use windows::Win32::System::Com::CLSCTX_ALL;
use windows::Win32::System::Threading::CreateEventW;

use crate::common::{map_hr, EventHandle};
use crate::lifecycle::{keepalive_error, StreamClient};

/// COM objects and the render event stay on the capture owner thread.
pub(crate) struct SilentRender {
    client: IAudioClient,
    render: IAudioRenderClient,
    pub(crate) event: EventHandle,
    buffer_frames: u32,
}

impl SilentRender {
    /// Use the exact endpoint and mix format already selected for capture.
    ///
    /// # Safety
    /// COM must be initialized here and `format` must be a valid mix format.
    pub(crate) unsafe fn new(device: &IMMDevice, format: *const WAVEFORMATEX) -> Result<Self> {
        // Microsoft, Loopback Recording:
        // "In loopback mode, a client of WASAPI can capture the audio stream
        // that is being played by a rendering endpoint device."
        // https://learn.microsoft.com/en-us/windows/win32/coreaudio/loopback-recording
        // The page describes capture of the render mix, but does not document
        // current idle packet suppression or recommend this silent keepalive.
        // On the measured Windows 11 endpoint, no render stream means no capture
        // packets. Our silence-only render stream keeps the engine clock running.
        // This is separate from that page's pre-1703 event-delivery workaround.
        let client: IAudioClient = device
            .Activate(CLSCTX_ALL, None)
            .map_err(|e| map_hr("silent keepalive Activate(IAudioClient)", e))?;
        client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                0,
                0,
                format,
                None,
            )
            .map_err(|e| map_hr("silent keepalive Initialize", e))?;
        let event = EventHandle(
            CreateEventW(None, false, false, PCWSTR::null())
                .map_err(|e| map_hr("silent keepalive CreateEventW", e))?,
        );
        client
            .SetEventHandle(event.0)
            .map_err(|e| map_hr("silent keepalive SetEventHandle", e))?;
        let render = client
            .GetService()
            .map_err(|e| map_hr("silent keepalive GetService(IAudioRenderClient)", e))?;
        let buffer_frames = client
            .GetBufferSize()
            .map_err(|e| map_hr("silent keepalive GetBufferSize", e))?;
        if buffer_frames == 0 {
            return Err(Error::Backend(
                "silent keepalive has an empty render buffer".into(),
            ));
        }
        Ok(Self {
            client,
            render,
            event,
            buffer_frames,
        })
    }
}

impl StreamClient for SilentRender {
    fn start(&mut self) -> Result<()> {
        unsafe { self.client.Start() }.map_err(|e| {
            keepalive_error(
                "cannot start classic loopback silent keepalive",
                map_hr("Start", e),
            )
        })
    }

    fn stop(&mut self) -> Result<()> {
        unsafe { self.client.Stop() }.map_err(|e| map_hr("silent keepalive Stop", e))
    }

    fn fill_silence(&mut self) -> Result<()> {
        unsafe {
            let padding = self
                .client
                .GetCurrentPadding()
                .map_err(|e| map_hr("silent keepalive GetCurrentPadding", e))?;
            let frames = self.buffer_frames.checked_sub(padding).ok_or_else(|| {
                Error::Backend("silent keepalive padding exceeds buffer size".into())
            })?;
            if frames != 0 {
                // ReleaseBuffer(SILENT) asks WASAPI to zero the entire acquired
                // buffer. Never write audio or modify endpoint/session volume.
                // https://learn.microsoft.com/en-us/windows/win32/api/audioclient/nf-audioclient-iaudiorenderclient-releasebuffer
                self.render
                    .GetBuffer(frames)
                    .map_err(|e| map_hr("silent keepalive GetBuffer", e))?;
                self.render
                    .ReleaseBuffer(frames, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)
                    .map_err(|e| map_hr("silent keepalive ReleaseBuffer", e))?;
            }
        }
        Ok(())
    }
}
