//! Device monitoring output and authoritative inventory reconciliation.
use flexaudio::core::{DeviceEvent, DeviceInfo, Error};

pub(super) fn report_device_event(
    event: DeviceEvent,
    relist: &mut impl FnMut() -> Result<Vec<DeviceInfo>, Error>,
) -> Result<(), String> {
    match event {
        DeviceEvent::Added(info) => {
            eprintln!(
                "[+] ADDED   {:<7} {} ({})",
                super::source_kind_label(info.source_kind),
                info.name,
                info.id,
            );
        }
        DeviceEvent::Removed { id } => eprintln!("[-] REMOVED {id}"),
        DeviceEvent::DefaultChanged { kind, id } => {
            eprintln!(
                "[*] DEFAULT {:<7} -> {id}",
                super::source_kind_label(kind.into())
            );
        }
        DeviceEvent::DefaultCleared { kind } => {
            eprintln!(
                "[*] DEFAULT {:<7} -> none",
                super::source_kind_label(kind.into())
            );
        }
        DeviceEvent::RescanRequired { dropped_events } => {
            eprintln!("Warning: device inventory invalidated ({dropped_events} dropped events); re-listing devices");
            rescan(relist)?;
        }
        _ => {
            eprintln!("[?] UNKNOWN device event; re-listing devices");
            rescan(relist)?;
        }
    }
    Ok(())
}

fn rescan(relist: &mut impl FnMut() -> Result<Vec<DeviceInfo>, Error>) -> Result<(), String> {
    let devices = relist().map_err(|error| format!("Failed to re-list devices: {error}"))?;
    eprintln!("[=] INVENTORY {} devices", devices.len());
    for device in devices {
        eprintln!(
            "[=] DEVICE  {:<7} {} ({}){}",
            super::source_kind_label(device.source_kind),
            device.name,
            device.id,
            if device.is_default { " [default]" } else { "" },
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use flexaudio::core::DefaultDeviceKind;

    #[test]
    fn rescan_relists_and_propagates_discovery_failure() {
        let mut calls = 0;
        let mut relist = || {
            calls += 1;
            Ok(Vec::new())
        };
        report_device_event(
            DeviceEvent::RescanRequired { dropped_events: 9 },
            &mut relist,
        )
        .unwrap();
        for kind in [
            DefaultDeviceKind::Microphone,
            DefaultDeviceKind::SystemAudio,
        ] {
            report_device_event(DeviceEvent::DefaultCleared { kind }, &mut relist).unwrap();
        }
        assert_eq!(calls, 1);
        let error = report_device_event(
            DeviceEvent::RescanRequired { dropped_events: 0 },
            &mut || Err(Error::Backend("device inventory query failed".into())),
        )
        .expect_err("failed discovery cannot become an empty success");
        assert!(error.contains("device inventory query failed"));
    }
}
