//! Read-only Core Audio activity adapter, called exclusively on the tap owner thread.

use std::ffi::c_void;
use std::ptr::NonNull;

use objc2_core_audio::{
    kAudioDevicePropertyDeviceUID, kAudioHardwarePropertyProcessObjectList,
    kAudioObjectPropertyElementMain, kAudioObjectPropertyScopeGlobal, kAudioProcessPropertyDevices,
    kAudioProcessPropertyIsRunningOutput, kAudioProcessPropertyPID, AudioObjectGetPropertyData,
    AudioObjectGetPropertyDataSize, AudioObjectID, AudioObjectPropertyAddress,
};

use crate::capture_health::{eligible_activity, ProcessActivity, Selection};
use crate::common::{read_cfstring_property, read_system_object_list, NO_ERR};
use crate::processes::{read_i32_property, read_u32_property};
use crate::tap::TapKind;

pub(crate) struct ActivityQuery {
    selection: Selection,
    device_uid: Option<String>,
    own_pid: u32,
}

impl ActivityQuery {
    pub(crate) fn new(kind: &TapKind) -> Self {
        let (selection, device_uid) = match kind {
            TapKind::IncludeProcesses(ids) => (Selection::Include(ids.clone()), None),
            TapKind::ExcludeProcesses(ids) => (Selection::Exclude(ids.clone()), None),
            TapKind::ExcludeProcessesOnDevice { ids, device_uid } => {
                (Selection::Exclude(ids.clone()), Some(device_uid.clone()))
            }
        };
        Self {
            selection,
            device_uid,
            own_pid: std::process::id(),
        }
    }

    pub(crate) fn poll(&self) -> Option<bool> {
        let objects = read_system_object_list(kAudioHardwarePropertyProcessObjectList).ok()?;
        let mut processes = Vec::with_capacity(objects.len());
        for object in objects {
            // An unreadable PID cannot safely be classified as excluded.
            let pid = u32::try_from(read_i32_property(object, kAudioProcessPropertyPID)?).ok()?;
            if pid == 0 {
                return None;
            }
            if !self.selection.includes(object, pid, self.own_pid) {
                continue;
            }
            let output_active = read_u32_property(object, kAudioProcessPropertyIsRunningOutput)
                .map(|value| value != 0);
            let on_device = match (&self.device_uid, output_active) {
                (Some(uid), Some(true)) => Some(process_uses_device(object, uid)?),
                _ => None,
            };
            processes.push(ProcessActivity {
                object,
                pid,
                output_active,
                on_device,
            });
        }
        eligible_activity(
            &self.selection,
            self.own_pid,
            self.device_uid.is_some(),
            &processes,
        )
    }
}

fn process_uses_device(object: AudioObjectID, selected_uid: &str) -> Option<bool> {
    let address = AudioObjectPropertyAddress {
        mSelector: kAudioProcessPropertyDevices,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    };
    let mut bytes: u32 = 0;
    // SAFETY: address and bytes are valid locals; this property takes no qualifier.
    let status = unsafe {
        AudioObjectGetPropertyDataSize(
            object,
            NonNull::from(&address),
            0,
            core::ptr::null(),
            NonNull::from(&mut bytes),
        )
    };
    let element = core::mem::size_of::<AudioObjectID>();
    if status != NO_ERR || bytes as usize % element != 0 {
        return None;
    }
    if bytes == 0 {
        return Some(false);
    }
    let mut devices = vec![0; bytes as usize / element];
    let capacity = bytes;
    // SAFETY: devices is aligned storage for capacity bytes. Core Audio may write at most
    // the advertised capacity; a changed property size returns a failure and disables inference.
    let status = unsafe {
        AudioObjectGetPropertyData(
            object,
            NonNull::from(&address),
            0,
            core::ptr::null(),
            NonNull::from(&mut bytes),
            NonNull::new(devices.as_mut_ptr().cast::<c_void>())?,
        )
    };
    if status != NO_ERR || bytes > capacity || bytes as usize % element != 0 {
        return None;
    }
    devices.truncate(bytes as usize / element);
    let mut matches = false;
    for device in devices {
        let uid = read_cfstring_property(
            device,
            kAudioDevicePropertyDeviceUID,
            kAudioObjectPropertyScopeGlobal,
        )?;
        matches |= uid == selected_uid;
    }
    Some(matches)
}
