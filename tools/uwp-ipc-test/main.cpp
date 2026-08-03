// UWP (Windows Store, AppContainerApplication=true) test app for the ALVR service IPC design.
//
// Verifies, from inside a genuinely packaged UWP AppContainer, that an app can:
//   1. open the service's named pipe   \\.\pipe\Global\alvr_spike1   (S-1-15-2-1 ACL)
//   2. open the service's named shared D3D11 texture by name         (S-1-15-2-1 ACL)
//   3. acquire/release the keyed mutex and write pixels the service can observe
//
// Results are written to the app's local state folder (a UWP app has no console), and also emitted
// via OutputDebugString so they can be watched live with DebugView / the VS output window.
//
// Run the desktop counterparts first:
//   spike1 server global      (pipe)
//   spike2 service            (texture)

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

constexpr wchar_t kPipeName[] = LR"(\\.\pipe\Global\alvr_spike1)";
constexpr wchar_t kTextureName[] = L"Global\\alvr_spike2_texture";
constexpr UINT kWidth = 64;
constexpr UINT kHeight = 64;

std::wstring g_log;

void Log(std::wstring_view line) {
    g_log += line;
    g_log += L"\r\n";
    OutputDebugStringW(std::wstring(line).c_str());
    OutputDebugStringW(L"\r\n");
}

std::wstring LastErrorText(DWORD error) {
    std::wostringstream out;
    out << L"error " << error;
    if (error == ERROR_ACCESS_DENIED) {
        out << L" (ACCESS_DENIED - the AppContainer grant is missing)";
    } else if (error == ERROR_FILE_NOT_FOUND) {
        out << L" (FILE_NOT_FOUND - is the desktop counterpart running?)";
    }
    return out.str();
}

/// Confirms we really are sandboxed, so a pass actually means something.
void ReportAppContainerState() {
    HANDLE token{};
    if (!OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &token)) {
        Log(L"[uwp] FAIL: OpenProcessToken");
        return;
    }

    DWORD isAppContainer = 0;
    DWORD size = 0;
    if (GetTokenInformation(token, TokenIsAppContainer, &isAppContainer,
                            sizeof(isAppContainer), &size)) {
        std::wostringstream out;
        out << L"[uwp] TokenIsAppContainer = " << isAppContainer << L" ("
            << (isAppContainer ? L"INSIDE an AppContainer" : L"NOT sandboxed") << L")";
        Log(out.str());
    } else {
        Log(L"[uwp] FAIL: GetTokenInformation(TokenIsAppContainer)");
    }
    CloseHandle(token);
}

bool TestPipe() {
    Log(L"[uwp] --- test 1: named pipe ---");
    std::wostringstream opening;
    opening << L"[uwp] opening " << kPipeName;
    Log(opening.str());

    // NOTE: CreateFileW is NOT part of the UWP/Store API surface -- it fails to compile in an
    // AppContainerApplication project. CreateFile2 is the supported equivalent and is what the
    // ALVR OpenXR runtime must use on the app side.
    CREATEFILE2_EXTENDED_PARAMETERS params{};
    params.dwSize = sizeof(params);
    params.dwFileAttributes = FILE_ATTRIBUTE_NORMAL;

    // A real client would retry / WaitNamedPipe here, since either side may start first.
    HANDLE pipe = CreateFile2(kPipeName, GENERIC_READ | GENERIC_WRITE, 0, OPEN_EXISTING, &params);
    if (pipe == INVALID_HANDLE_VALUE) {
        std::wostringstream out;
        out << L"[uwp] FAIL: CreateFileW: " << LastErrorText(GetLastError());
        Log(out.str());
        return false;
    }
    Log(L"[uwp] OK: pipe opened");

    const std::string message = "hello from packaged UWP pid " + std::to_string(GetCurrentProcessId());
    DWORD written = 0;
    if (!WriteFile(pipe, message.data(), static_cast<DWORD>(message.size()), &written, nullptr)) {
        std::wostringstream out;
        out << L"[uwp] FAIL: WriteFile: " << LastErrorText(GetLastError());
        Log(out.str());
        CloseHandle(pipe);
        return false;
    }

    std::wostringstream out;
    out << L"[uwp] OK: wrote " << written << L" bytes";
    Log(out.str());
    Log(L"[uwp] PASS: pipe reachable from UWP");
    CloseHandle(pipe);
    return true;
}

bool TestSharedTexture() {
    Log(L"[uwp] --- test 2: named shared texture ---");

    com_ptr<ID3D11Device> device;
    com_ptr<ID3D11DeviceContext> context;
    HRESULT hr = D3D11CreateDevice(nullptr, D3D_DRIVER_TYPE_HARDWARE, nullptr, 0, nullptr, 0,
                                   D3D11_SDK_VERSION, device.put(), nullptr, context.put());
    if (FAILED(hr)) {
        std::wostringstream out;
        out << L"[uwp] FAIL: D3D11CreateDevice hr=0x" << std::hex << hr;
        Log(out.str());
        return false;
    }
    Log(L"[uwp] OK: D3D11 hardware device created inside the sandbox");

    auto device1 = device.as<ID3D11Device1>();

    com_ptr<ID3D11Texture2D> shared;
    // Access flags must match what the service granted, and the HRESULT must be checked -- the
    // SimulatedReality reference does neither and only gets away with it by ignoring the result.
    hr = device1->OpenSharedResourceByName(kTextureName,
                                           DXGI_SHARED_RESOURCE_READ | DXGI_SHARED_RESOURCE_WRITE,
                                           __uuidof(ID3D11Texture2D), shared.put_void());
    if (FAILED(hr)) {
        std::wostringstream out;
        out << L"[uwp] FAIL: OpenSharedResourceByName hr=0x" << std::hex << hr;
        if (hr == E_ACCESSDENIED) {
            out << L" (ACCESS_DENIED - the AppContainer grant is missing)";
        }
        Log(out.str());
        return false;
    }
    Log(L"[uwp] OK: shared texture opened by name");

    // Source full of a distinctive colour to blit in.
    std::vector<uint8_t> pixels(kWidth * kHeight * 4, 0xAA);
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
    hr = device->CreateTexture2D(&desc, &init, source.put());
    if (FAILED(hr)) {
        Log(L"[uwp] FAIL: CreateTexture2D(source)");
        return false;
    }

    auto mutex = shared.as<IDXGIKeyedMutex>();

    // App side of the ping-pong: acquire on 1, release on 0 (service uses {0,1}).
    bool wrote = false;
    for (int attempt = 0; attempt < 50 && !wrote; ++attempt) {
        if (mutex->AcquireSync(1, 100) == S_OK) {
            context->CopyResource(shared.get(), source.get());
            context->Flush();
            mutex->ReleaseSync(0);

            std::wostringstream out;
            out << L"[uwp] OK: wrote pixels through the keyed mutex (attempt " << attempt << L")";
            Log(out.str());
            wrote = true;
        } else {
            Sleep(100);
        }
    }

    if (!wrote) {
        Log(L"[uwp] FAIL: never acquired the keyed mutex");
        return false;
    }

    Log(L"[uwp] PASS: shared texture writable from UWP");
    return true;
}

// Note: CoreApplication::Exit() can tear the app down before this async write completes, so treat
// the desktop counterparts' output as the authoritative result and this file as a convenience.
void WriteResultsFile() {
    try {
        auto folder = winrt::Windows::Storage::ApplicationData::Current().LocalFolder();
        auto file = folder.CreateFileAsync(L"alvr_uwp_ipc_test.log",
                                           winrt::Windows::Storage::CreationCollisionOption::ReplaceExisting)
                        .get();
        winrt::Windows::Storage::FileIO::WriteTextAsync(file, g_log).get();

        std::wostringstream out;
        out << L"[uwp] results written to " << folder.Path().c_str() << L"\\alvr_uwp_ipc_test.log";
        OutputDebugStringW(out.str().c_str());
    } catch (...) {
        OutputDebugStringW(L"[uwp] FAIL: could not write results file\r\n");
    }
}

void RunTests() {
    Log(L"=== ALVR UWP IPC test (packaged Windows Store app) ===");
    ReportAppContainerState();

    const bool pipeOk = TestPipe();
    const bool textureOk = TestSharedTexture();

    Log(L"");
    Log(pipeOk && textureOk ? L"[uwp] OVERALL: PASS" : L"[uwp] OVERALL: FAIL");

    WriteResultsFile();
}

/// Minimal IFrameworkView: UWP requires a view, but this app is a test harness with no UI.
struct App : implements<App, IFrameworkViewSource, IFrameworkView> {
    IFrameworkView CreateView() { return *this; }
    void Initialize(CoreApplicationView const&) {}
    void Load(hstring const&) {}
    void Uninitialize() {}

    void SetWindow(CoreWindow const&) {}

    void Run() {
        auto window = CoreWindow::GetForCurrentThread();
        window.Activate();

        RunTests();

        // Exit immediately: this is a test, not an interactive app.
        CoreApplication::Exit();
    }
};

} // namespace

// C++/WinRT UWP apps use wWinMain (C++/CX would use main(Platform::Array<String^>^) instead).
int __stdcall wWinMain(HINSTANCE, HINSTANCE, PWSTR, int) {
    init_apartment();
    CoreApplication::Run(make<App>());
    return 0;
}
