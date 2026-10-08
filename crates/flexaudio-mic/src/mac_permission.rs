//! Public AVFoundation and main-bundle microphone permission adapter.

use std::sync::OnceLock;

use block2::RcBlock;
use objc2::runtime::Bool;
use objc2::{class, msg_send};
use objc2_foundation::{ns_string, NSBundle, NSString};

use crate::mac_policy::{self, PromptCoordinator, Provider, Status};
use flexaudio_core::types::{Error, Result};

static PROMPTS: OnceLock<PromptCoordinator> = OnceLock::new();

#[link(name = "AVFoundation", kind = "framework")]
extern "C" {
    // This public SDK constant has type AVMediaType (an NSString pointer).
    static AVMediaTypeAudio: &'static NSString;
}

pub(crate) struct Native;

impl Provider for Native {
    fn status(&self) -> Result<Status> {
        // SAFETY: AVMediaTypeAudio is the framework's immutable audio media-type
        // constant and is valid for this public, thread-safe authorization query.
        // AVAuthorizationStatus is an NSInteger in the public SDK (isize here).
        let status: isize = unsafe {
            msg_send![class!(AVCaptureDevice), authorizationStatusForMediaType: AVMediaTypeAudio]
        };
        match status {
            3 => Ok(Status::Authorized),
            2 => Ok(Status::Denied),
            1 => Ok(Status::Restricted),
            0 => Ok(Status::NotDetermined),
            _ => Err(Error::Backend(
                "AVFoundation returned an unknown microphone authorization status".into(),
            )),
        }
    }

    fn has_usage_description(&self) -> Result<bool> {
        let value = NSBundle::mainBundle()
            .objectForInfoDictionaryKey(ns_string!("NSMicrophoneUsageDescription"));
        Ok(value
            .and_then(|value| value.downcast::<NSString>().ok())
            .is_some_and(|value| !value.to_string().trim().is_empty()))
    }

    fn request_access(&self, completion: Box<dyn Fn(bool) + Send + Sync>) -> Result<()> {
        // The coordinator calls this only after validating the main bundle's usage
        // description. AVFoundation copies the block; its captured callback owns
        // the shared prompt until completion, even when the waiter times out.
        let block = RcBlock::new(move |granted: Bool| completion(granted.as_bool()));
        // SAFETY: The audio constant is valid and the block has the exact SDK ABI.
        // AVFoundation retains/copies it for its asynchronous completion queue.
        unsafe {
            let _: () = msg_send![class!(AVCaptureDevice), requestAccessForMediaType: AVMediaTypeAudio, completionHandler: &*block];
        }
        Ok(())
    }
}

pub(crate) fn preflight() -> Result<bool> {
    mac_policy::preflight(
        &Native,
        PROMPTS.get_or_init(PromptCoordinator::default),
        mac_policy::PROMPT_TIMEOUT,
    )
}

pub(crate) fn can_query_format() -> bool {
    matches!(Native.status(), Ok(Status::Authorized))
}
