//! Windows process-loopback capture and app listing. Runs only inside the
//! helper process (see the parent module), never in the recorder itself.

use std::collections::HashMap;
use std::io::Write;
use std::mem::ManuallyDrop;
use std::sync::Mutex;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK, AUDIOCLIENT_ACTIVATION_PARAMS,
    AUDIOCLIENT_ACTIVATION_PARAMS_0, AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
    AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS, ActivateAudioInterfaceAsync, AudioSessionStateActive,
    DEVICE_STATE_ACTIVE, IActivateAudioInterfaceAsyncOperation,
    IActivateAudioInterfaceCompletionHandler, IActivateAudioInterfaceCompletionHandler_Impl,
    IAudioCaptureClient, IAudioClient, IAudioSessionControl2, IAudioSessionManager2,
    IMMDeviceEnumerator, MMDeviceEnumerator, PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
    PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE, VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
    WAVEFORMATEX, eRender,
};
use windows::Win32::System::Com::StructuredStorage::{
    PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0,
};
use windows::Win32::System::Com::{
    BLOB, CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{CreateEventW, GetCurrentProcessId, WaitForSingleObject};
use windows::Win32::System::Variant::VT_BLOB;
use windows::core::{Interface, Ref, implement};

use super::{APP_CHANNELS, APP_SAMPLE_RATE, AppInfo, AppTarget, FrameWriter, HEARTBEAT};

/// How often the target app's processes are looked up again.
const RECHECK: Duration = Duration::from_secs(2);
const WAVE_FORMAT_PCM: u16 = 1;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;

fn com_init() -> Result<(), String> {
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
        .ok()
        .map_err(|e| format!("COM init failed: {e}"))
}

struct Process {
    pid: u32,
    parent: u32,
    exe: String,
}

fn processes() -> Result<Vec<Process>, String> {
    let mut found = Vec::new();
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)
            .map_err(|e| format!("cannot list processes: {e}"))?;
        let mut entry = PROCESSENTRY32W {
            dwSize: size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut ok = Process32FirstW(snap, &mut entry).is_ok();
        while ok {
            let len = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(0);
            found.push(Process {
                pid: entry.th32ProcessID,
                parent: entry.th32ParentProcessID,
                exe: String::from_utf16_lossy(&entry.szExeFile[..len]),
            });
            ok = Process32NextW(snap, &mut entry).is_ok();
        }
        let _ = CloseHandle(snap);
    }
    Ok(found)
}

/// The topmost running process of `exe` (its parent is a different program),
/// whose process tree holds all of the app's audio. Lowest PID if several.
fn root_process(exe: &str) -> Result<Option<u32>, String> {
    let all = processes()?;
    let by_pid: HashMap<u32, &Process> = all.iter().map(|p| (p.pid, p)).collect();
    let mut roots: Vec<u32> = all
        .iter()
        .filter(|p| p.exe.eq_ignore_ascii_case(exe))
        .filter(|p| {
            by_pid
                .get(&p.parent)
                .is_none_or(|parent| !parent.exe.eq_ignore_ascii_case(exe))
        })
        .map(|p| p.pid)
        .collect();
    roots.sort_unstable();
    Ok(roots.first().copied())
}

/// Programs with an audio session on any active output device.
pub fn list_apps() -> Result<Vec<AppInfo>, String> {
    com_init()?;
    let names: HashMap<u32, String> = processes()?.into_iter().map(|p| (p.pid, p.exe)).collect();
    let mut apps = Vec::new();
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .map_err(|e| format!("no audio service: {e}"))?;
        let devices = enumerator
            .EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)
            .map_err(|e| e.to_string())?;
        for i in 0..devices.GetCount().unwrap_or(0) {
            let Ok(device) = devices.Item(i) else {
                continue;
            };
            let Ok(manager) = device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) else {
                continue;
            };
            let Ok(sessions) = manager.GetSessionEnumerator() else {
                continue;
            };
            for j in 0..sessions.GetCount().unwrap_or(0) {
                let Ok(session) = sessions.GetSession(j) else {
                    continue;
                };
                let Ok(session) = session.cast::<IAudioSessionControl2>() else {
                    continue;
                };
                let pid = session.GetProcessId().unwrap_or(0);
                // PID 0 is the system sounds session.
                let Some(exe) = names.get(&pid).filter(|_| pid != 0) else {
                    continue;
                };
                let playing = session
                    .GetState()
                    .is_ok_and(|s| s == AudioSessionStateActive);
                apps.push(AppInfo {
                    exe: exe.clone(),
                    playing,
                });
            }
        }
    }
    Ok(apps)
}

#[implement(IActivateAudioInterfaceCompletionHandler)]
struct Activated(Mutex<Option<mpsc::Sender<()>>>);

impl IActivateAudioInterfaceCompletionHandler_Impl for Activated_Impl {
    fn ActivateCompleted(
        &self,
        _operation: Ref<IActivateAudioInterfaceAsyncOperation>,
    ) -> windows::core::Result<()> {
        if let Some(tx) = self.0.lock().ok().and_then(|mut t| t.take()) {
            let _ = tx.send(());
        }
        Ok(())
    }
}

/// An audio client capturing `pid`'s process tree (or everything else).
fn activate(pid: u32, exclude: bool) -> Result<IAudioClient, String> {
    let mut params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: pid,
                ProcessLoopbackMode: if exclude {
                    PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE
                } else {
                    PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE
                },
            },
        },
    };
    // Points at `params`, which outlives the call. PROPVARIANT's contents are
    // ManuallyDrop, so the blob is never freed by Windows' allocator.
    let prop = PROPVARIANT {
        Anonymous: PROPVARIANT_0 {
            Anonymous: ManuallyDrop::new(PROPVARIANT_0_0 {
                vt: VT_BLOB,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: PROPVARIANT_0_0_0 {
                    blob: BLOB {
                        cbSize: size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
                        pBlobData: (&raw mut params).cast(),
                    },
                },
            }),
        },
    };
    let (tx, rx) = mpsc::channel();
    let handler: IActivateAudioInterfaceCompletionHandler = Activated(Mutex::new(Some(tx))).into();
    let operation = unsafe {
        ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some(&prop),
            &handler,
        )
    }
    .map_err(|e| unsupported(&e))?;
    rx.recv_timeout(Duration::from_secs(5))
        .map_err(|_| "Windows did not answer the capture request".to_string())?;
    let mut result = windows::core::HRESULT(0);
    let mut client = None;
    unsafe { operation.GetActivateResult(&mut result, &mut client) }
        .map_err(|e| unsupported(&e))?;
    result.ok().map_err(|e| unsupported(&e))?;
    client
        .ok_or_else(|| "no audio client".to_string())?
        .cast::<IAudioClient>()
        .map_err(|e| e.to_string())
}

fn unsupported(e: &windows::core::Error) -> String {
    format!(
        "cannot record apps on this Windows version ({e}); it needs Windows 11 or a recent Windows 10"
    )
}

fn format(tag: u16, bits: u16) -> WAVEFORMATEX {
    let block = APP_CHANNELS * bits / 8;
    WAVEFORMATEX {
        wFormatTag: tag,
        nChannels: APP_CHANNELS,
        nSamplesPerSec: APP_SAMPLE_RATE,
        nAvgBytesPerSec: APP_SAMPLE_RATE * u32::from(block),
        nBlockAlign: block,
        wBitsPerSample: bits,
        cbSize: 0,
    }
}

/// Captures until the target app closes (or appears, for "everything
/// except"), the recorder goes away, or Windows fails. Returns a message for
/// the recorder's log in those cases.
pub fn capture(target: &AppTarget, out: &mut FrameWriter<impl Write>) -> Result<(), String> {
    com_init()?;
    let found = root_process(&target.exe)?;
    let pid = match (found, target.exclude) {
        (Some(pid), _) => pid,
        (None, false) => return Err(format!("{} is not running", target.app_name())),
        // Excluding an app that isn't running: exclude only this helper,
        // which plays nothing, i.e. record everything.
        (None, true) => unsafe { GetCurrentProcessId() },
    };
    let client = activate(pid, target.exclude)?;
    let flags = AUDCLNT_STREAMFLAGS_LOOPBACK
        | AUDCLNT_STREAMFLAGS_EVENTCALLBACK
        | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM;
    // 200 ms buffer, in 100 ns units. Float is preferred; 16-bit PCM is the
    // format Microsoft's own sample uses, as a fallback.
    let mut float = true;
    let init = |f: &WAVEFORMATEX| unsafe {
        client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 2_000_000, 0, f, None)
    };
    if init(&format(WAVE_FORMAT_IEEE_FLOAT, 32)).is_err() {
        float = false;
        init(&format(WAVE_FORMAT_PCM, 16)).map_err(|e| format!("cannot start capture: {e}"))?;
    }
    let event: HANDLE =
        unsafe { CreateEventW(None, false, false, None) }.map_err(|e| e.to_string())?;
    let capture: IAudioCaptureClient;
    unsafe {
        client.SetEventHandle(event).map_err(|e| e.to_string())?;
        capture = client.GetService().map_err(|e| e.to_string())?;
        client
            .Start()
            .map_err(|e| format!("cannot start capture: {e}"))?;
    }
    let what = if target.exclude {
        "everything except"
    } else {
        "capturing"
    };
    let _ = out.info(&format!("{what} {} (process {pid})", target.exe));
    let gone = |e: std::io::Error| format!("recorder went away: {e}");
    out.heartbeat().map_err(gone)?;

    let mut samples: Vec<f32> = Vec::new();
    let mut last_beat = Instant::now();
    let mut last_check = Instant::now();
    loop {
        let wait = unsafe { WaitForSingleObject(event, HEARTBEAT.as_millis() as u32) };
        if wait == WAIT_OBJECT_0 {
            loop {
                let frames = unsafe { capture.GetNextPacketSize() }.map_err(|e| e.to_string())?;
                if frames == 0 {
                    break;
                }
                let (mut data, mut frames, mut flags) = (std::ptr::null_mut(), 0u32, 0u32);
                unsafe { capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None) }
                    .map_err(|e| format!("capture failed: {e}"))?;
                let n = frames as usize * usize::from(APP_CHANNELS);
                samples.clear();
                if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                    samples.resize(n, 0.0);
                } else if float {
                    let s = unsafe { std::slice::from_raw_parts(data.cast::<f32>(), n) };
                    samples.extend_from_slice(s);
                } else {
                    let s = unsafe { std::slice::from_raw_parts(data.cast::<i16>(), n) };
                    samples.extend(s.iter().map(|&v| f32::from(v) / 32768.0));
                }
                unsafe { capture.ReleaseBuffer(frames) }.map_err(|e| e.to_string())?;
                out.audio(&samples).map_err(gone)?;
            }
        }
        if last_beat.elapsed() >= HEARTBEAT {
            last_beat = Instant::now();
            out.heartbeat().map_err(gone)?;
        }
        if last_check.elapsed() >= RECHECK {
            last_check = Instant::now();
            let now = root_process(&target.exe)?;
            match (target.exclude, now) {
                (false, None) => return Err(format!("{} was closed", target.app_name())),
                (false, Some(p)) if p != pid => {
                    return Err(format!("{} restarted", target.app_name()));
                }
                // A newly started (or restarted) app to exclude: start over so
                // its sound is left out.
                (true, Some(p)) if p != pid => {
                    return Err(format!("restarting to leave out {}", target.app_name()));
                }
                _ => {}
            }
        }
    }
}
