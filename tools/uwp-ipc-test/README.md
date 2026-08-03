# alvr_uwp_ipc_test

A **real packaged UWP app** (`ApplicationType = Windows Store`, `AppContainerApplication = true`) that
verifies the ALVR service's IPC design is reachable from a sandboxed OpenXR host app.

Tests, from inside the UWP sandbox:

1. opening the service's named pipe `\\.\pipe\Global\alvr_spike1`
2. opening the service's named shared D3D11 texture by name
3. acquiring the keyed mutex and writing pixels the service can observe

This complements [`../appcontainer-run`](../appcontainer-run), which fakes the sandbox by launching a
desktop binary inside an AppContainer. That harness is quicker to iterate on, but it uses the
**desktop** API surface, so it cannot catch Store-API restrictions. This project can — see the
finding below.

## Finding: `CreateFileW` is not available to UWP

`CreateFileW` is **not** part of the UWP/Store API surface and fails to compile in an
`AppContainerApplication` project:

```
error C3861: 'CreateFileW': identifier not found
```

`CreateFile2` is the supported equivalent. **The ALVR OpenXR runtime must use `CreateFile2` on the app
side of the pipe.** Only a real UWP build surfaces this; the AppContainer harness compiled `CreateFileW`
happily.

## Build and run

```powershell
$mb = "C:\Program Files\Microsoft Visual Studio\2022\Professional\MSBuild\Current\Bin\MSBuild.exe"
& $mb alvr_uwp_ipc_test.vcxproj /p:Configuration=Debug /p:Platform=x64

# Register the loose layout (developer mode; no signing required)
Add-AppxPackage -Register x64\Debug\alvr_uwp_ipc_test\AppxManifest.xml

# Start the desktop counterparts first, then launch the app
$fam = (Get-AppxPackage -Name ALVR.UwpIpcTest).PackageFamilyName
Start-Process "shell:AppsFolder\$fam!App"
```

Read results from the **desktop counterparts' stdout** — that is the authoritative signal. The app also
tries to write `LocalState\alvr_uwp_ipc_test.log` and emits `OutputDebugString` (visible in DebugView),
but `CoreApplication::Exit()` can cut the async file write short.

## Project notes

- `CompileAsWinRT=false` — this is C++/WinRT, not C++/CX. With CX enabled, `vccorlib` expects a
  `main(Platform::Array<String^>^)` entry point and the link fails.
- Entry point is `wWinMain`, correct for C++/WinRT.
- `mp:PhoneIdentity` is required or packaging fails with `APPX1673`.
- **No capabilities are declared**, deliberately. The design's claim is that reaching the service's
  named objects needs only the service-side `S-1-15-2-1` ACL, not an app capability. If this app ever
  needs a capability to pass, that is a finding worth knowing.
- Logo PNGs under `Assets/` are placeholder solid colours, generated only to satisfy the manifest.
