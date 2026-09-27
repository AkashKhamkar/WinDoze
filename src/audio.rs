//! Which processes are currently producing sound (so we never freeze music / calls).

use std::collections::HashSet;

use windows::Win32::Media::Audio::Endpoints::IAudioMeterInformation;
use windows::Win32::Media::Audio::{
    AudioSessionStateActive, DEVICE_STATE_ACTIVE, IAudioSessionControl2, IAudioSessionManager2, IMMDeviceEnumerator,
    MMDeviceEnumerator, eRender,
};
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};
use windows::core::Interface;

/// PIDs with an active audio session whose meter is above silence, on any output device.
/// The engine thread must have called CoInitializeEx.
pub fn pids_playing_audio() -> HashSet<u32> {
    let mut out = HashSet::new();
    if let Err(e) = collect(&mut out) {
        crate::logln!("audio check failed: {e}");
    }
    out
}

fn collect(out: &mut HashSet<u32>) -> windows::core::Result<()> {
    unsafe {
        let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let devices = enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
        for d in 0..devices.GetCount()? {
            let Ok(device) = devices.Item(d) else { continue };
            let Ok(manager) = device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) else { continue };
            let Ok(sessions) = manager.GetSessionEnumerator() else { continue };
            for i in 0..sessions.GetCount()? {
                let Ok(session) = sessions.GetSession(i) else { continue };
                if session.GetState().ok() != Some(AudioSessionStateActive) {
                    continue;
                }
                let Ok(control2) = session.cast::<IAudioSessionControl2>() else { continue };
                let Ok(pid) = control2.GetProcessId() else { continue };
                let loud = session
                    .cast::<IAudioMeterInformation>()
                    .and_then(|m| m.GetPeakValue())
                    .map(|peak| peak > 0.0005)
                    .unwrap_or(false);
                if loud && pid != 0 {
                    out.insert(pid);
                }
            }
        }
    }
    Ok(())
}
