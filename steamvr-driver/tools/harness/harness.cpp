// Test harness: drives a SteamVR server driver the way vrserver does and writes a text trace.
//
// One source, two builds:
//   * x86_64-w64-mingw32-g++  -> harness.exe, run under Wine against a Windows driver_simracingstudio.dll
//   * g++ (Linux)             -> harness,     run against the Linux driver_simracingstudio.so
// The trace records interface requests, property writes, device registration and every
// DriverPose_t the driver submits (wall-clock times and pointer values are masked when traces are compared).
//
// usage: harness <scenario> <trace-file> <driver-library>
// Shared memory: on Windows the harness creates the named sections the motion app creates
// (page-file backed, read-write, of the given size and name); on Linux it creates the mirror
// files under $OPENRIGCUE_TEST_MIRROR_DIR (test build of the driver only).

#include <cerrno>
#include <cstdarg>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <cmath>
#include <string>

#ifdef _WIN32
#include <windows.h>
#else
#include <dlfcn.h>
#include <fcntl.h>
#include <sys/mman.h>
#include <unistd.h>
#include <signal.h>
#endif

namespace vr {
typedef int EVRInitError;
typedef int ETrackedDeviceClass;
typedef int ETrackedPropertyError;
typedef uint64_t PropertyContainerHandle_t;
struct DriverPose_t { unsigned char bytes[280]; };
struct PropertyWrite_t {
    int32_t prop;
    int32_t writeType;
    int32_t eSetError;
    void* pvBuffer;
    uint32_t unBufferSize;
    uint32_t unTag;
    int32_t eError;
};
struct PropertyRead_t;

class IVRDriverContext {
public:
    virtual void* GetGenericInterface(const char* pchInterfaceVersion, EVRInitError* peError) = 0;
    virtual uint64_t GetDriverHandle() = 0;
};
class ITrackedDeviceServerDriver {
public:
    virtual EVRInitError Activate(uint32_t unObjectId) = 0;
    virtual void Deactivate() = 0;
    virtual void EnterStandby() = 0;
    virtual void* GetComponent(const char* pchComponentNameAndVersion) = 0;
    virtual void DebugRequest(const char* pchRequest, char* pchResponseBuffer, uint32_t unResponseBufferSize) = 0;
    virtual DriverPose_t GetPose() = 0;
};
class IServerTrackedDeviceProvider {
public:
    virtual EVRInitError Init(IVRDriverContext* pDriverContext) = 0;
    virtual void Cleanup() = 0;
    virtual const char* const* GetInterfaceVersions() = 0;
    virtual void RunFrame() = 0;
    virtual bool ShouldBlockStandbyMode() = 0;
    virtual void EnterStandby() = 0;
    virtual void LeaveStandby() = 0;
};
class IVRWatchdogProvider {
public:
    virtual EVRInitError Init(IVRDriverContext* pDriverContext) = 0;
    virtual void Cleanup() = 0;
};
}  // namespace vr

static FILE* g_trace;
static void T(const char* fmt, ...) __attribute__((format(printf, 1, 2)));
static void T(const char* fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    vfprintf(g_trace, fmt, ap);
    va_end(ap);
    fputc('\n', g_trace);
    fflush(g_trace);
}
static std::string hex(const void* p, size_t n) {
    static const char* d = "0123456789abcdef";
    std::string s;
    const unsigned char* b = (const unsigned char*)p;
    for (size_t i = 0; i < n; i++) { s += d[b[i] >> 4]; s += d[b[i] & 15]; }
    return s;
}

// ---------------------------------------------------------------------------------------------------- fakes
// Any slot the driver never calls traps into one of these, so an unexpected call shows up in the trace.
#define UNEXPECTED(n) virtual void unexpected##n() { T("UNEXPECTED %s slot %d", name, n); }
struct Generic {
    const char* name;
    explicit Generic(const char* n) : name(n) {}
    UNEXPECTED(0) UNEXPECTED(1) UNEXPECTED(2) UNEXPECTED(3) UNEXPECTED(4) UNEXPECTED(5) UNEXPECTED(6) UNEXPECTED(7)
    UNEXPECTED(8) UNEXPECTED(9) UNEXPECTED(10) UNEXPECTED(11) UNEXPECTED(12) UNEXPECTED(13) UNEXPECTED(14) UNEXPECTED(15)
};

static vr::ITrackedDeviceServerDriver* g_device;

struct FakeHost {
    const char* name = "IVRServerDriverHost_006";
    virtual bool TrackedDeviceAdded(const char* serial, vr::ETrackedDeviceClass cls, vr::ITrackedDeviceServerDriver* drv) {
        T("host.TrackedDeviceAdded serial=\"%s\" class=%d driver=%s", serial, cls, drv ? "nonnull" : "null");
        g_device = drv;
        return true;
    }
    virtual void TrackedDevicePoseUpdated(uint32_t id, const vr::DriverPose_t& pose, uint32_t size) {
        T("host.TrackedDevicePoseUpdated id=%u size=%u pose=%s", id, size, hex(&pose, sizeof pose).c_str());
    }
    UNEXPECTED(2) UNEXPECTED(3) UNEXPECTED(4) UNEXPECTED(5) UNEXPECTED(6) UNEXPECTED(7)
    UNEXPECTED(8) UNEXPECTED(9) UNEXPECTED(10) UNEXPECTED(11) UNEXPECTED(12) UNEXPECTED(13) UNEXPECTED(14) UNEXPECTED(15)
};

struct FakeProps {
    const char* name = "IVRProperties_001";
    virtual vr::ETrackedPropertyError ReadPropertyBatch(vr::PropertyContainerHandle_t, vr::PropertyRead_t*, uint32_t) {
        T("UNEXPECTED props.ReadPropertyBatch");
        return 0;
    }
    virtual vr::ETrackedPropertyError WritePropertyBatch(vr::PropertyContainerHandle_t c, vr::PropertyWrite_t* b, uint32_t n) {
        T("props.WritePropertyBatch container=0x%llx count=%u", (unsigned long long)c, n);
        for (uint32_t i = 0; i < n; i++) {
            T("  prop=%d writeType=%d size=%u tag=%u buffer=%s", b[i].prop, b[i].writeType, b[i].unBufferSize, b[i].unTag,
              b[i].pvBuffer ? hex(b[i].pvBuffer, b[i].unBufferSize).c_str() : "null");
            b[i].eSetError = 0;
            b[i].eError = 0;
        }
        return 0;
    }
    virtual const char* GetPropErrorNameFromEnum(vr::ETrackedPropertyError) {
        T("UNEXPECTED props.GetPropErrorNameFromEnum");
        return "";
    }
    virtual vr::PropertyContainerHandle_t TrackedDeviceToPropertyContainer(uint32_t idx) {
        T("props.TrackedDeviceToPropertyContainer %u", idx);
        return 0x1234560000000000ULL | idx;
    }
    UNEXPECTED(4) UNEXPECTED(5) UNEXPECTED(6) UNEXPECTED(7)
};

static FakeHost g_host;
static FakeProps g_props;
static Generic g_settings("IVRSettings_003"), g_log("IVRDriverLog_001"), g_manager("IVRDriverManager_001"),
    g_resources("IVRResources_001"), g_watchdog_host("IVRWatchdogHost_002"), g_other("other");

struct FakeContext : vr::IVRDriverContext {
    const char* missing = nullptr;  // interface name answered with NULL
    void* GetGenericInterface(const char* v, vr::EVRInitError* err) override {
        void* r = nullptr;
        if (!strcmp(v, "IVRServerDriverHost_006")) r = &g_host;
        else if (!strcmp(v, "IVRProperties_001")) r = &g_props;
        else if (!strcmp(v, "IVRSettings_003")) r = &g_settings;
        else if (!strcmp(v, "IVRDriverLog_001")) r = &g_log;
        else if (!strcmp(v, "IVRDriverManager_001")) r = &g_manager;
        else if (!strcmp(v, "IVRResources_001")) r = &g_resources;
        else if (!strcmp(v, "IVRWatchdogHost_002")) r = &g_watchdog_host;
        else r = &g_other;
        if (missing && !strcmp(v, missing)) r = nullptr;
        T("ctx.GetGenericInterface \"%s\" err_ptr=%s -> %s", v, err ? "nonnull" : "null", r ? "object" : "null");
        if (err) *err = r ? 0 : 105;
        return r;
    }
    uint64_t GetDriverHandle() override {
        T("UNEXPECTED ctx.GetDriverHandle");
        return 0;
    }
};

// ---------------------------------------------------------------------------------------------------- platform
typedef void* (*Factory)(const char*, int*);
static Factory g_factory;

#ifdef _WIN32
static HMODULE g_driver;
static void load(const char* path) {
    HMODULE m = LoadLibraryA(path);
    if (!m) { T("LoadLibrary failed %lu", GetLastError()); exit(2); }
    g_factory = (Factory)GetProcAddress(m, "HmdDriverFactory");
    g_driver = m;
}
// Wine 10 reports a read of a mapped page whose file was truncated (SIGBUS) as EXCEPTION_ACCESS_VIOLATION;
// Windows reports it as EXCEPTION_IN_PAGE_ERROR, which is what a Windows driver's exception filter accepts. While the
// backing is gone, this vectored handler (it runs before any frame-based handler) gives such a fault inside the
// driver the Windows exception code, with the Windows parameter layout (access, address, NTSTATUS).
static volatile LONG g_backing_gone;
static LONG CALLBACK in_page_veh(EXCEPTION_POINTERS* ep) {
    EXCEPTION_RECORD* r = ep->ExceptionRecord;
    if (!g_backing_gone || r->ExceptionCode != EXCEPTION_ACCESS_VIOLATION) return EXCEPTION_CONTINUE_SEARCH;
    DWORD last_error = GetLastError();  // the driver logs GetLastError() in its catch block: leave it as it was
    HMODULE m = nullptr;
    BOOL found = GetModuleHandleExA(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                                    (LPCSTR)r->ExceptionAddress, &m);
    SetLastError(last_error);
    if (!found || m != g_driver) return EXCEPTION_CONTINUE_SEARCH;
    r->ExceptionCode = EXCEPTION_IN_PAGE_ERROR;
    r->NumberParameters = 3;
    r->ExceptionInformation[2] = 0xC000020C;  // STATUS_CONNECTION_DISCONNECTED (any I/O status)
    return EXCEPTION_CONTINUE_SEARCH;
}
static double* section(const char* name, size_t size) {
    wchar_t w[128];
    MultiByteToWideChar(CP_UTF8, 0, name, -1, w, 128);
    HANDLE h = CreateFileMappingW(INVALID_HANDLE_VALUE, NULL, PAGE_READWRITE, 0, (DWORD)size, w);
    if (!h) { T("CreateFileMapping failed %lu", GetLastError()); exit(2); }
    return (double*)MapViewOfFile(h, FILE_MAP_ALL_ACCESS, 0, 0, size);
}
// A named section backed by the file "rig.bin" in the working directory, so that its backing store can be taken
// away (EXCEPTION_IN_PAGE_ERROR on the next read of a view). Windows refuses to truncate a mapped file, so the
// truncation is done by a helper shell outside Wine: backing(n) creates "req<n>" and waits for
// "ack<n>"; odd n truncates rig.bin to 0 bytes, even n extends it back to 4096.
static double* file_section(const char* name) {
    wchar_t w[128];
    AddVectoredExceptionHandler(1, in_page_veh);
    MultiByteToWideChar(CP_UTF8, 0, name, -1, w, 128);
    HANDLE f = CreateFileW(L"rig.bin", GENERIC_READ | GENERIC_WRITE, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                           NULL, CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL, NULL);
    if (f == INVALID_HANDLE_VALUE) { T("CreateFile failed %lu", GetLastError()); exit(2); }
    HANDLE h = CreateFileMappingW(f, NULL, PAGE_READWRITE, 0, 4096, w);
    if (!h) { T("CreateFileMapping failed %lu", GetLastError()); exit(2); }
    return (double*)MapViewOfFile(h, FILE_MAP_ALL_ACCESS, 0, 0, 4096);
}
static void backing(int n) {
    char req[16], ack[16];
    snprintf(req, sizeof req, "req%d", n);
    snprintf(ack, sizeof ack, "ack%d", n);
    FILE* f = fopen(req, "wb");
    if (f) fclose(f);
    for (int i = 0; i < 400; i++) {
        if (GetFileAttributesA(ack) != INVALID_FILE_ATTRIBUTES) { g_backing_gone = n & 1; return; }
        Sleep(50);
    }
    T("backing %d timeout", n);
    exit(2);
}
// Sets the thread's last error before a frame (ERROR_ACCESS_DENIED), to see what the failed frame logs from it.
static void preset_last_error() { SetLastError(ERROR_ACCESS_DENIED); }
static vr::DriverPose_t get_pose(vr::ITrackedDeviceServerDriver* d) {
    // MSVC member-function ABI: this in RCX, hidden return pointer in RDX.
    typedef vr::DriverPose_t* (*Fn)(void*, vr::DriverPose_t*);
    vr::DriverPose_t out;
    ((Fn)(*(void***)d)[5])(d, &out);
    return out;
}
static void raise_in_driver_crt(int sig) {
    typedef int (*RaiseFn)(int);
    RaiseFn r = (RaiseFn)GetProcAddress(GetModuleHandleA("ucrtbase.dll"), "raise");
    r(sig);
}
#else
static void load(const char* path) {
    void* m = dlopen(path, RTLD_NOW | RTLD_LOCAL);
    if (!m) { T("dlopen failed %s", dlerror()); exit(2); }
    g_factory = (Factory)dlsym(m, "HmdDriverFactory");
}
static double* section(const char* name, size_t size) {
    const char* dir = getenv("OPENRIGCUE_TEST_MIRROR_DIR");
    std::string p = std::string(dir ? dir : ".") + "/";
    for (const char* c = name; *c; c++) p += (*c == '\\' || *c == '/') ? '_' : *c;
    int fd = open(p.c_str(), O_RDWR | O_CREAT | O_TRUNC, 0644);
    if (fd < 0 || ftruncate(fd, (off_t)size) != 0) { T("cannot create %s", p.c_str()); exit(2); }
    void* m = mmap(nullptr, size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    close(fd);
    return (double*)m;
}
// backing(n): odd n creates the test build's "<mirror file>.in-page-error" marker, which makes reads of the section's
// views fail (the Linux form of a section whose backing store cannot be read); even n removes it.
static std::string g_backing;
static double* file_section(const char* name) {
    const char* dir = getenv("OPENRIGCUE_TEST_MIRROR_DIR");
    g_backing = std::string(dir ? dir : ".") + "/";
    for (const char* c = name; *c; c++) g_backing += (*c == '\\' || *c == '/') ? '_' : *c;
    return section(name, 4096);
}
static void backing(int n) {
    std::string marker = g_backing + ".in-page-error";
    if (n & 1) {
        FILE* f = fopen(marker.c_str(), "wb");
        if (!f) { T("backing %d failed", n); exit(2); }
        fclose(f);
    } else if (unlink(marker.c_str()) != 0) {
        T("backing %d failed", n);
        exit(2);
    }
}
// errno is the last error on Linux: EACCES for ERROR_ACCESS_DENIED.
static void preset_last_error() { errno = EACCES; }
static vr::DriverPose_t get_pose(vr::ITrackedDeviceServerDriver* d) { return d->GetPose(); }
static void raise_in_driver_crt(int sig) { raise(sig); }
#endif

// ---------------------------------------------------------------------------------------------------- scenarios
static vr::IServerTrackedDeviceProvider* provider() {
    int rc = -1;
    void* p = g_factory("IServerTrackedDeviceProvider_004", &rc);
    T("factory IServerTrackedDeviceProvider_004 -> %s rc=%d", p ? "object" : "null", rc);
    return (vr::IServerTrackedDeviceProvider*)p;
}
static vr::IVRWatchdogProvider* watchdog() {
    int rc = -1;
    void* p = g_factory("IVRWatchdogProvider_001", &rc);
    T("factory IVRWatchdogProvider_001 -> %s rc=%d", p ? "object" : "null", rc);
    return (vr::IVRWatchdogProvider*)p;
}
static void frame(vr::IServerTrackedDeviceProvider* p, const char* label) {
    T("RunFrame %s", label);
    p->RunFrame();
}
static void set6(double* s, double a, double b, double c, double d, double e, double f) {
    s[0] = a; s[1] = b; s[2] = c; s[3] = d; s[4] = e; s[5] = f;
}

static void scenario_main() {
    int rc = -1;
    void* p = g_factory("Bogus_001", &rc);
    T("factory Bogus_001 -> %s rc=%d", p ? "object" : "null", rc);
    p = g_factory("Bogus_001", nullptr);
    T("factory Bogus_001 (null rc) -> %s", p ? "object" : "null");
    vr::IServerTrackedDeviceProvider* prov = provider();
    vr::IServerTrackedDeviceProvider* prov2 = provider();
    T("provider stable=%d", prov == prov2);
    vr::IVRWatchdogProvider* wd = watchdog();
    T("watchdog distinct=%d", (void*)wd != (void*)prov);
    const char* const* v = prov->GetInterfaceVersions();
    for (int i = 0; v[i]; i++) T("interface[%d]=\"%s\"", i, v[i]);
    T("ShouldBlockStandbyMode=%d", (int)prov->ShouldBlockStandbyMode());
    prov->EnterStandby();
    prov->LeaveStandby();
    frame(prov, "before-init");
    prov->Cleanup();
    T("Cleanup before init done");

    FakeContext ctx;
    rc = prov->Init(&ctx);
    T("Init -> %d", rc);
    if (!g_device) { T("no device"); return; }
    rc = g_device->Activate(7);
    T("Activate(7) -> %d", rc);
    T("GetComponent -> %s", g_device->GetComponent("IVRDisplayComponent_002") ? "nonnull" : "null");
    char buf[4] = {'a', 'b', 'c', 0};
    g_device->DebugRequest("req", buf, 4);
    T("DebugRequest(4) buf=%s", hex(buf, 4).c_str());
    buf[0] = 'z';
    g_device->DebugRequest("req", buf, 0);
    T("DebugRequest(0) buf=%s", hex(buf, 4).c_str());
    g_device->EnterStandby();

    frame(prov, "no-sections");
    frame(prov, "no-sections-2");
    vr::DriverPose_t direct = get_pose(g_device);
    T("GetPose direct pose=%s", hex(&direct, sizeof direct).c_str());

    double* rig = section("Local\\SimRacingStudioMotionRigPose", 48);
    set6(rig, 12.5, -30.25, 7.75, 15.0, -4.5, 3.25);
    frame(prov, "rig-created");
    set6(rig, 0, 0, 0, 0, 0, 0);
    frame(prov, "rig-zero");
    set6(rig, -100.0, 250.5, -12.125, 90.0, 30.0, -45.0);
    frame(prov, "rig-values-2");
    set6(rig, 1e-3, -1e-3, 333.333, 720.0, -180.0, 179.999);
    frame(prov, "rig-values-3");
    set6(rig, -0.0, -0.0, -0.0, -0.0, -0.0, -0.0);
    frame(prov, "rig-neg-zero");
    set6(rig, 5e-324, -5e-324, 1e300, 1e-300, 1e20, -1e20);
    frame(prov, "rig-extremes");
    set6(rig, INFINITY, -INFINITY, NAN, 1.0, NAN, INFINITY);
    frame(prov, "rig-nonfinite");

    double* ovrmc = section("Local\\OVRMC_MMFv1", 4096);
    ovrmc[0] = 0.25; ovrmc[1] = -0.5; ovrmc[2] = 1.125; ovrmc[3] = 9.0; ovrmc[4] = 0.7; ovrmc[5] = 8.0;
    set6(rig, 12.5, -30.25, 7.75, 15.0, -4.5, 3.25);
    frame(prov, "ovrmc-created");
    frame(prov, "ovrmc-applied");
    ovrmc[0] = -1.5; ovrmc[1] = 2.25; ovrmc[2] = -3.0; ovrmc[4] = -2.0;
    frame(prov, "ovrmc-changed");
    for (int i = 0; i < 37; i++) {
        double t = i * 0.37;
        set6(rig, 40 * sin(t), 25 * cos(1.3 * t), 10 * sin(2.1 * t), 20 * sin(0.7 * t), 8 * cos(0.9 * t), 6 * sin(1.7 * t));
        ovrmc[4] = 0.05 * i;
        char label[32];
        snprintf(label, sizeof label, "sweep-%02d", i);
        frame(prov, label);
    }
    direct = get_pose(g_device);
    T("GetPose direct pose=%s", hex(&direct, sizeof direct).c_str());

    g_device->Deactivate();
    T("Deactivate done");
    frame(prov, "after-deactivate");
    rc = g_device->Activate(3);
    T("Activate(3) -> %d", rc);
    frame(prov, "after-reactivate");

    prov->Cleanup();
    T("Cleanup done");
    frame(prov, "after-cleanup");
    prov->Cleanup();
    T("Cleanup again done");

    g_device = nullptr;
    rc = prov->Init(&ctx);
    T("Init again -> %d", rc);
    if (g_device) {
        frame(prov, "second-device-before-activate");
        rc = g_device->Activate(9);
        T("Activate(9) -> %d", rc);
        frame(prov, "second-device");
    }
    prov->Cleanup();
    T("Cleanup done");
}

// Rig section present before the device exists.
static void scenario_early() {
    double* rig = section("Local\\SimRacingStudioMotionRigPose", 48);
    set6(rig, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0);
    vr::IServerTrackedDeviceProvider* prov = provider();
    FakeContext ctx;
    int rc = prov->Init(&ctx);
    T("Init -> %d", rc);
    frame(prov, "before-activate");
    rc = g_device->Activate(1);
    T("Activate(1) -> %d", rc);
    frame(prov, "first");
    frame(prov, "second");
    prov->Cleanup();
    T("Cleanup done");
}

static void scenario_init_failures() {
    vr::IServerTrackedDeviceProvider* prov = provider();
    const char* names[] = {"IVRServerDriverHost_006", "IVRSettings_003", "IVRProperties_001", "IVRDriverLog_001",
                           "IVRDriverManager_001", "IVRResources_001"};
    for (const char* n : names) {
        FakeContext ctx;
        ctx.missing = n;
        g_device = nullptr;
        T("--- missing %s", n);
        int rc = prov->Init(&ctx);
        T("Init -> %d device=%s", rc, g_device ? "added" : "none");
        if (g_device && strcmp(n, "IVRProperties_001") != 0) {
            frame(prov, "after-init");
        }
        prov->Cleanup();
        T("Cleanup done");
    }
    vr::IVRWatchdogProvider* wd = watchdog();
    const char* wnames[] = {"", "IVRWatchdogHost_002", "IVRSettings_003", "IVRDriverLog_001"};
    for (const char* n : wnames) {
        FakeContext ctx;
        ctx.missing = n[0] ? n : nullptr;
        T("--- watchdog missing %s", n[0] ? n : "(none)");
        int rc = wd->Init(&ctx);
        T("watchdog Init -> %d", rc);
        wd->Cleanup();
        T("watchdog Cleanup done");
    }
    // The watchdog shares the driver context globals with the provider: after its Init the provider's
    // RunFrame uses whatever the watchdog left (here: host pointer cleared, context = ctx2).
    FakeContext ctx;
    g_device = nullptr;
    int rc = prov->Init(&ctx);
    T("Init -> %d", rc);
    rc = g_device->Activate(4);
    T("Activate(4) -> %d", rc);
    FakeContext ctx2;
    rc = wd->Init(&ctx2);
    T("watchdog Init -> %d", rc);
    frame(prov, "after-watchdog-init");
    prov->Cleanup();
    T("Cleanup done");
}

// The catch path of RunFrame: the rig section's backing store disappears while the driver has a view of it.
static void scenario_inpage() {
    vr::IServerTrackedDeviceProvider* prov = provider();
    FakeContext ctx;
    int rc = prov->Init(&ctx);
    T("Init -> %d", rc);
    rc = g_device->Activate(5);
    T("Activate(5) -> %d", rc);
    double* rig = file_section("Local\\SimRacingStudioMotionRigPose");
    set6(rig, 12.5, -30.25, 7.75, 15.0, -4.5, 3.25);
    frame(prov, "connected");
    frame(prov, "connected-2");
    backing(1);
    frame(prov, "in-page-error");      // OVRMC never connected: its handle is NULL
    frame(prov, "in-page-error-2");    // reconnects the rig section, faults again
    backing(2);
    set6(rig, -100.0, 250.5, -12.125, 90.0, 30.0, -45.0);
    frame(prov, "restored");
    double* ovrmc = section("Local\\OVRMC_MMFv1", 4096);
    ovrmc[0] = 0.25; ovrmc[1] = -0.5; ovrmc[2] = 1.125; ovrmc[3] = 9.0; ovrmc[4] = 0.7; ovrmc[5] = 8.0;
    frame(prov, "ovrmc-connected");
    frame(prov, "ovrmc-applied");
    backing(3);
    preset_last_error();
    frame(prov, "in-page-error-both");  // both handles open: both close
    frame(prov, "in-page-error-both-2");
    backing(4);
    set6(rig, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0);
    frame(prov, "restored-2");
    frame(prov, "restored-3");
    prov->Cleanup();
    T("Cleanup done");
}

static void scenario_load_only() { T("loaded"); }

static void scenario_crash(int sig) {
    T("raising %d", sig);
    raise_in_driver_crt(sig);
    T("returned from raise");
}

int main(int argc, char** argv) {
    if (argc < 4) return 1;
    g_trace = fopen(argv[2], "wb");
    if (!g_trace) return 1;
    load(argv[3]);
    T("HmdDriverFactory %s", g_factory ? "exported" : "missing");
    std::string s = argv[1];
    if (s == "main") scenario_main();
    else if (s == "early") scenario_early();
    else if (s == "init-failures") scenario_init_failures();
    else if (s == "load") scenario_load_only();
    else if (s == "inpage") scenario_inpage();
    else if (s == "crash-segv") scenario_crash(11);
    else if (s == "crash-fpe") scenario_crash(8);
    else if (s == "crash-ill") scenario_crash(4);
    else if (s == "crash-int") scenario_crash(2);
    T("end");
    fclose(g_trace);
    return 0;
}
