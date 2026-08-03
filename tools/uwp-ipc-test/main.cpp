// UWP (Windows Store, AppContainerApplication=true, NO runFullTrust) probe for reaching a desktop
// service from inside a real AppContainer.
//
// The question this answers: which IPC mechanism can a genuinely sandboxed OpenXR app use to reach
// the ALVR desktop service? Named pipes are documented as unavailable to Store apps
// (CreateFile2: "You can't open named pipes"), so this tests every plausible alternative side by
// side and reports which actually work.
//
// Each probe targets an object the DESKTOP SERVICE created, ACL'd to grant S-1-15-2-1
// (ALL_APPLICATION_PACKAGES). Run `uwp_probe_host` on the desktop first; it creates all of them.
//
// Results go to the app's LocalState folder AND OutputDebugString, because a UWP app has no console.
// The host process also reports what it observed, which is the authoritative signal.

#include <winrt/Windows.Foundation.h>
#include <winrt/Windows.Storage.h>
#include <winrt/Windows.ApplicationModel.Core.h>
#include <winrt/Windows.UI.Core.h>

#include <d3d11_1.h>
#include <dxgi1_2.h>
#include <windows.h>

#include <string>
#include <sstream>
#include <vector>

#pragma comment(lib, "d3d11.lib")

using namespace winrt;
using namespace winrt::Windows::ApplicationModel::Core;
using namespace winrt::Windows::Foundation;
using namespace winrt::Windows::UI::Core;

namespace {

// All names are in Global\ and created by the desktop host with an S-1-15-2-1 grant.
constexpr wchar_t kPipeName[] = LR"(\\.\pipe\Global\alvr_probe_pipe)";
// Session-local, NOT Global\: creating a section object or event in Global\ needs
// SeCreateGlobalPrivilege, which an unelevated service does not have (measured: CreateFileMapping in
// Global\ fails with ACCESS_DENIED). Pipes are different -- there Global\ is just part of the name.
constexpr wchar_t kSharedMemName[] = L"alvr_probe_shmem";
constexpr wchar_t kRequestEventName[] = L"alvr_probe_request";
constexpr wchar_t kReplyEventName[] = L"alvr_probe_reply";
constexpr wchar_t kTextureName[] = L"alvr_probe_texture";

constexpr UINT kWidth = 64;
constexpr UINT kHeight = 64;

std::wstring g_log;

void Log(std::wstring_view line) {
    g_log += line;
    g_log += L"\r\n";
    OutputDebugStringW(std::wstring(line).c_str());
    OutputDebugStringW(L"\r\n");
}

std::wstring ErrorText(DWORD error) {
    std::wostringstream out;
    out << L"error " << error;
    switch (error) {
        case ERROR_ACCESS_DENIED:
            out << L" (ACCESS_DENIED - missing AppContainer grant)";
            break;
        case ERROR_FILE_NOT_FOUND:
            out << L" (FILE_NOT_FOUND - host not running, or name not visible here)";
            break;
        case ERROR_NOT_SUPPORTED_IN_APPCONTAINER:
            out << L" (NOT_SUPPORTED_IN_APPCONTAINER - blocked by sandbox policy)";
            break;
        default:
            break;
    }
    return out.str();
}

void ReportSandboxState() {
    HANDLE token{};
    if (!OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &token)) {
        Log(L"[probe] FAIL: OpenProcessToken");
        return;
    }

    DWORD isAppContainer = 0;
    DWORD size = 0;
    if (GetTokenInformation(token, TokenIsAppContainer, &isAppContainer,
                            sizeof(isAppContainer), &size)) {
        std::wostringstream out;
        out << L"[probe] TokenIsAppContainer = " << isAppContainer << L" ("
            << (isAppContainer ? L"SANDBOXED - results are meaningful"
                               : L"NOT sandboxed - results prove nothing!")
            << L")";
        Log(out.str());
    }
    CloseHandle(token);
}

/// Probe 1: named pipe. Expected to FAIL - Store apps cannot open named pipes.
bool ProbeNamedPipe() {
    Log(L"[probe] --- 1. named pipe (expected to fail) ---");

    CREATEFILE2_EXTENDED_PARAMETERS params{};
    params.dwSize = sizeof(params);
    params.dwFileAttributes = FILE_ATTRIBUTE_NORMAL;

    // CreateFileW is not in the UWP API surface at all (error C3861), so CreateFile2 is the only
    // option here -- and its docs say it cannot open named pipes from a Store app.
    HANDLE pipe = CreateFile2(kPipeName, GENERIC_READ | GENERIC_WRITE, 0, OPEN_EXISTING, &params);
    if (pipe == INVALID_HANDLE_VALUE) {
        std::wostringstream out;
        out << L"[probe] pipe: FAIL - " << ErrorText(GetLastError());
        Log(out.str());
        return false;
    }

    const std::string message = "pipe from UWP pid " + std::to_string(GetCurrentProcessId());
    DWORD written = 0;
    const bool ok = WriteFile(pipe, message.data(), static_cast<DWORD>(message.size()),
                              &written, nullptr);
    CloseHandle(pipe);

    Log(ok ? L"[probe] pipe: PASS (unexpected - pipes work from AppContainer!)"
           : L"[probe] pipe: opened but write failed");
    return ok;
}

/// Probe 2: shared memory + events. The most promising candidate: Spike 2 already showed an
/// AppContainer can open a service-created, ACL'd named object.
bool ProbeSharedMemory() {
    Log(L"[probe] --- 2. shared memory + named events ---");

    // OpenFileMappingFromApp is the Store-approved entry point (OpenFileMappingW is not).
    HANDLE mapping = OpenFileMappingFromApp(FILE_MAP_ALL_ACCESS, FALSE, kSharedMemName);
    if (!mapping) {
        std::wostringstream out;
        out << L"[probe] shmem: FAIL to open mapping - " << ErrorText(GetLastError());
        Log(out.str());
        return false;
    }
    Log(L"[probe] shmem: OK opened mapping");

    void* view = MapViewOfFileFromApp(mapping, FILE_MAP_ALL_ACCESS, 0, 4096);
    if (!view) {
        std::wostringstream out;
        out << L"[probe] shmem: FAIL to map view - " << ErrorText(GetLastError());
        Log(out.str());
        CloseHandle(mapping);
        return false;
    }
    Log(L"[probe] shmem: OK mapped view");

    // Events are how the two sides signal each other; without them shared memory needs polling.
    //
    // There is no OpenEventFromApp. OpenEventW is not in the UWP surface either, but CreateEventExW
    // IS -- and without CREATE_NEW it opens an existing named event, which is what we need.
    HANDLE request = CreateEventExW(nullptr, kRequestEventName, 0,
                                    EVENT_MODIFY_STATE | SYNCHRONIZE);
    HANDLE reply = CreateEventExW(nullptr, kReplyEventName, 0, SYNCHRONIZE);

    if (!request || !reply) {
        std::wostringstream out;
        out << L"[probe] shmem: FAIL to open events - " << ErrorText(GetLastError());
        Log(out.str());
        UnmapViewOfFile(view);
        CloseHandle(mapping);
        return false;
    }
    Log(L"[probe] shmem: OK opened both events");

    // Write a request, signal the host, wait for its reply. This is a full round trip, which is what
    // a control channel actually needs -- not just "the object opened".
    const std::string message = "shmem from UWP pid " + std::to_string(GetCurrentProcessId());
    memcpy(view, message.data(), message.size() + 1);

    SetEvent(request);
    Log(L"[probe] shmem: signalled request, waiting up to 5s for reply...");

    const DWORD waited = WaitForSingleObject(reply, 5000);
    bool ok = false;
    if (waited == WAIT_OBJECT_0) {
        // The host writes its answer back into the same buffer.
        const char* answer = static_cast<const char*>(view);
        std::wostringstream out;
        out << L"[probe] shmem: PASS - round trip complete, host replied: "
            << std::wstring(answer, answer + strlen(answer)).c_str();
        Log(out.str());
        ok = true;
    } else {
        std::wostringstream out;
        out << L"[probe] shmem: FAIL - no reply (wait returned " << waited << L")";
        Log(out.str());
    }

    CloseHandle(request);
    CloseHandle(reply);
    UnmapViewOfFile(view);
    CloseHandle(mapping);
    return ok;
}

/// Probe 3: named shared D3D11 texture + keyed mutex. This is the frame path.
bool ProbeSharedTexture() {
    Log(L"[probe] --- 3. named shared D3D11 texture ---");

    com_ptr<ID3D11Device> device;
    com_ptr<ID3D11DeviceContext> context;
    HRESULT hr = D3D11CreateDevice(nullptr, D3D_DRIVER_TYPE_HARDWARE, nullptr, 0, nullptr, 0,
                                   D3D11_SDK_VERSION, device.put(), nullptr, context.put());
    if (FAILED(hr)) {
        std::wostringstream out;
        out << L"[probe] texture: FAIL D3D11CreateDevice hr=0x" << std::hex << hr;
        Log(out.str());
        return false;
    }
    Log(L"[probe] texture: OK D3D11 hardware device created inside the sandbox");

    auto device1 = device.as<ID3D11Device1>();

    com_ptr<ID3D11Texture2D> shared;
    hr = device1->OpenSharedResourceByName(kTextureName,
                                          DXGI_SHARED_RESOURCE_READ | DXGI_SHARED_RESOURCE_WRITE,
                                          __uuidof(ID3D11Texture2D), shared.put_void());
    if (FAILED(hr)) {
        std::wostringstream out;
        out << L"[probe] texture: FAIL OpenSharedResourceByName hr=0x" << std::hex << hr;
        if (hr == E_ACCESSDENIED) out << L" (ACCESS_DENIED)";
        Log(out.str());
        return false;
    }
    Log(L"[probe] texture: OK opened by name");

    std::vector<uint8_t> pixels(kWidth * kHeight * 4, 0xC3);
    D3D11_TEXTURE2D_DESC desc{};
    desc.Width = kWidth;
    desc.Height = kHeight;
    desc.MipLevels = 1;
    desc.ArraySize = 1;
    desc.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
    desc.SampleDesc.Count = 1;
    desc.Usage = D3D11_USAGE_DEFAULT;
    desc.BindFlags = D3D11_BIND_SHADER_RESOURCE;

    D3D11_SUBRESOURCE_DATA init{};
    init.pSysMem = pixels.data();
    init.SysMemPitch = kWidth * 4;

    com_ptr<ID3D11Texture2D> source;
    if (FAILED(device->CreateTexture2D(&desc, &init, source.put()))) {
        Log(L"[probe] texture: FAIL CreateTexture2D(source)");
        return false;
    }

    auto mutex = shared.as<IDXGIKeyedMutex>();
    for (int attempt = 0; attempt < 50; ++attempt) {
        if (mutex->AcquireSync(1, 100) == S_OK) {
            context->CopyResource(shared.get(), source.get());
            context->Flush();
            mutex->ReleaseSync(0);
            Log(L"[probe] texture: PASS - wrote 0xC3 pixels through the keyed mutex");
            return true;
        }
        Sleep(100);
    }

    Log(L"[probe] texture: FAIL - never acquired the keyed mutex");
    return false;
}

/// Probe 4: loopback TCP. The SimulatedReality reference's choice. Needs
/// privateNetworkClientServer, which this manifest deliberately does NOT declare, so a failure here
/// tells us whether the capability is genuinely required.
bool ProbeLoopbackTcp() {
    Log(L"[probe] --- 4. loopback TCP (no capability declared) ---");
    // Deliberately not implemented: adding Winsock here would confound the result with capability
    // and loopback-exemption issues, which are a separate question. Recorded as untested.
    Log(L"[probe] tcp: SKIPPED - see notes; test separately with the capability declared");
    return false;
}

void WriteResultsFile() {
    try {
        auto folder = winrt::Windows::Storage::ApplicationData::Current().LocalFolder();
        auto file = folder
                        .CreateFileAsync(L"alvr_uwp_probe.log",
                                         winrt::Windows::Storage::CreationCollisionOption::ReplaceExisting)
                        .get();
        winrt::Windows::Storage::FileIO::WriteTextAsync(file, g_log).get();
    } catch (...) {
        OutputDebugStringW(L"[probe] could not write results file\r\n");
    }
}

void RunProbes() {
    Log(L"=== ALVR UWP -> desktop service IPC probe ===");
    ReportSandboxState();

    const bool pipe = ProbeNamedPipe();
    const bool shmem = ProbeSharedMemory();
    const bool texture = ProbeSharedTexture();
    const bool tcp = ProbeLoopbackTcp();

    Log(L"");
    Log(L"=== SUMMARY ===");
    Log(pipe ? L"  named pipe     : WORKS" : L"  named pipe     : blocked");
    Log(shmem ? L"  shared memory  : WORKS" : L"  shared memory  : blocked");
    Log(texture ? L"  shared texture : WORKS" : L"  shared texture : blocked");
    Log(tcp ? L"  loopback TCP   : WORKS" : L"  loopback TCP   : untested");

    if (shmem && texture) {
        Log(L"  => viable design: shared memory for control, shared texture for frames");
    } else if (!shmem && !texture && !pipe) {
        Log(L"  => nothing worked; is uwp_probe_host running on the desktop?");
    }

    WriteResultsFile();
}

/// Minimal IFrameworkView: UWP needs a view, but this is a headless probe.
struct App : implements<App, IFrameworkViewSource, IFrameworkView> {
    IFrameworkView CreateView() { return *this; }
    void Initialize(CoreApplicationView const&) {}
    void Load(hstring const&) {}
    void Uninitialize() {}
    void SetWindow(CoreWindow const&) {}

    void Run() {
        auto window = CoreWindow::GetForCurrentThread();
        window.Activate();

        RunProbes();

        CoreApplication::Exit();
    }
};

} // namespace

// C++/WinRT UWP apps use wWinMain (C++/CX would use main(Platform::Array<String^>^)).
int __stdcall wWinMain(HINSTANCE, HINSTANCE, PWSTR, int) {
    init_apartment();
    CoreApplication::Run(make<App>());
    return 0;
}
