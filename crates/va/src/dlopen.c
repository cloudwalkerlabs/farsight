// libva, libva-drm and libdrm, loaded at the first call rather than when
// the binary starts. The binaries then run without them (VA-API simply
// fails, and the client decodes in software), and use the system's libva,
// which has to match its drivers: a driver only initialises from a libva
// at least as new as the one it was built against, so a static copy would
// age out.
//
// Each function here stands in for libva's or libdrm's own, for both
// farsight-va and the static FFmpeg; the compiler checks them against the
// headers. Add one when FFmpeg starts calling it: the link then fails
// without it.

#include <dlfcn.h>
#include <errno.h>
#include <pthread.h>
#include <stddef.h>

#include <va/va.h>
#include <va/va_drm.h>
#include <va/va_str.h>
#include <xf86drm.h>

enum { LIBVA, LIBVA_DRM, LIBDRM };

static void *libs[3];
static pthread_once_t loaded = PTHREAD_ONCE_INIT;

static void load(void) {
    libs[LIBVA] = dlopen("libva.so.2", RTLD_NOW | RTLD_LOCAL);
    libs[LIBVA_DRM] = libs[LIBVA] ? dlopen("libva-drm.so.2", RTLD_NOW | RTLD_LOCAL) : NULL;
    libs[LIBDRM] = dlopen("libdrm.so.2", RTLD_NOW | RTLD_LOCAL);
}

// The library's own definition, or NULL. Lookups through the handle find
// the library's, never these.
static void *resolve(int lib, const char *name) {
    pthread_once(&loaded, load);
    return libs[lib] ? dlsym(libs[lib], name) : NULL;
}

#define CALL(lib, name, args, fail)                                            \
    static __typeof__(&name) fn;                                               \
    __typeof__(&name) f = __atomic_load_n(&fn, __ATOMIC_ACQUIRE);              \
    if (!f) {                                                                  \
        f = (__typeof__(&name))resolve(lib, #name);                            \
        if (!f)                                                                \
            return fail;                                                       \
        __atomic_store_n(&fn, f, __ATOMIC_RELEASE);                            \
    }                                                                          \
    return f args;

#define STUB(lib, ret, name, params, args, fail)                               \
    ret name params { CALL(lib, name, args, fail) }

#define VA(name, params, args) STUB(LIBVA, VAStatus, name, params, args, VA_STATUS_ERROR_UNKNOWN)

// clang-format off
VA(vaInitialize, (VADisplay dpy, int *major, int *minor), (dpy, major, minor))
VA(vaTerminate, (VADisplay dpy), (dpy))
VA(vaSetDriverName, (VADisplay dpy, char *name), (dpy, name))
VA(vaQueryConfigProfiles, (VADisplay dpy, VAProfile *list, int *n), (dpy, list, n))
VA(vaQueryConfigEntrypoints, (VADisplay dpy, VAProfile profile, VAEntrypoint *list, int *n), (dpy, profile, list, n))
VA(vaGetConfigAttributes, (VADisplay dpy, VAProfile profile, VAEntrypoint entrypoint, VAConfigAttrib *list, int n), (dpy, profile, entrypoint, list, n))
VA(vaCreateConfig, (VADisplay dpy, VAProfile profile, VAEntrypoint entrypoint, VAConfigAttrib *list, int n, VAConfigID *config), (dpy, profile, entrypoint, list, n, config))
VA(vaDestroyConfig, (VADisplay dpy, VAConfigID config), (dpy, config))
VA(vaQuerySurfaceAttributes, (VADisplay dpy, VAConfigID config, VASurfaceAttrib *list, unsigned int *n), (dpy, config, list, n))
VA(vaCreateSurfaces, (VADisplay dpy, unsigned int format, unsigned int width, unsigned int height, VASurfaceID *surfaces, unsigned int n, VASurfaceAttrib *attribs, unsigned int n_attribs), (dpy, format, width, height, surfaces, n, attribs, n_attribs))
VA(vaDestroySurfaces, (VADisplay dpy, VASurfaceID *surfaces, int n), (dpy, surfaces, n))
VA(vaCreateContext, (VADisplay dpy, VAConfigID config, int width, int height, int flag, VASurfaceID *targets, int n, VAContextID *context), (dpy, config, width, height, flag, targets, n, context))
VA(vaDestroyContext, (VADisplay dpy, VAContextID context), (dpy, context))
VA(vaCreateBuffer, (VADisplay dpy, VAContextID context, VABufferType type, unsigned int size, unsigned int n, void *data, VABufferID *buf), (dpy, context, type, size, n, data, buf))
VA(vaMapBuffer, (VADisplay dpy, VABufferID buf, void **p), (dpy, buf, p))
VA(vaUnmapBuffer, (VADisplay dpy, VABufferID buf), (dpy, buf))
VA(vaDestroyBuffer, (VADisplay dpy, VABufferID buf), (dpy, buf))
VA(vaAcquireBufferHandle, (VADisplay dpy, VABufferID buf, VABufferInfo *info), (dpy, buf, info))
VA(vaReleaseBufferHandle, (VADisplay dpy, VABufferID buf), (dpy, buf))
VA(vaExportSurfaceHandle, (VADisplay dpy, VASurfaceID surface, uint32_t mem_type, uint32_t flags, void *descriptor), (dpy, surface, mem_type, flags, descriptor))
VA(vaBeginPicture, (VADisplay dpy, VAContextID context, VASurfaceID target), (dpy, context, target))
VA(vaRenderPicture, (VADisplay dpy, VAContextID context, VABufferID *bufs, int n), (dpy, context, bufs, n))
VA(vaEndPicture, (VADisplay dpy, VAContextID context), (dpy, context))
VA(vaSyncSurface, (VADisplay dpy, VASurfaceID target), (dpy, target))
VA(vaSyncBuffer, (VADisplay dpy, VABufferID buf, uint64_t timeout_ns), (dpy, buf, timeout_ns))
VA(vaQueryImageFormats, (VADisplay dpy, VAImageFormat *list, int *n), (dpy, list, n))
VA(vaCreateImage, (VADisplay dpy, VAImageFormat *format, int width, int height, VAImage *image), (dpy, format, width, height, image))
VA(vaDestroyImage, (VADisplay dpy, VAImageID image), (dpy, image))
VA(vaGetImage, (VADisplay dpy, VASurfaceID surface, int x, int y, unsigned int width, unsigned int height, VAImageID image), (dpy, surface, x, y, width, height, image))
VA(vaPutImage, (VADisplay dpy, VASurfaceID surface, VAImageID image, int src_x, int src_y, unsigned int src_width, unsigned int src_height, int dest_x, int dest_y, unsigned int dest_width, unsigned int dest_height), (dpy, surface, image, src_x, src_y, src_width, src_height, dest_x, dest_y, dest_width, dest_height))
VA(vaDeriveImage, (VADisplay dpy, VASurfaceID surface, VAImage *image), (dpy, surface, image))

STUB(LIBVA, int, vaMaxNumProfiles, (VADisplay dpy), (dpy), 0)
STUB(LIBVA, int, vaMaxNumEntrypoints, (VADisplay dpy), (dpy), 0)
STUB(LIBVA, int, vaMaxNumImageFormats, (VADisplay dpy), (dpy), 0)
STUB(LIBVA, const char *, vaQueryVendorString, (VADisplay dpy), (dpy), NULL)
STUB(LIBVA, const char *, vaErrorStr, (VAStatus status), (status), "libva is not installed")
STUB(LIBVA, const char *, vaProfileStr, (VAProfile profile), (profile), "<unknown profile>")
STUB(LIBVA, const char *, vaEntrypointStr, (VAEntrypoint entrypoint), (entrypoint), "<unknown entrypoint>")
STUB(LIBVA, VAMessageCallback, vaSetErrorCallback, (VADisplay dpy, VAMessageCallback cb, void *ctx), (dpy, cb, ctx), NULL)
STUB(LIBVA, VAMessageCallback, vaSetInfoCallback, (VADisplay dpy, VAMessageCallback cb, void *ctx), (dpy, cb, ctx), NULL)

STUB(LIBVA_DRM, VADisplay, vaGetDisplayDRM, (int fd), (fd), NULL)

STUB(LIBDRM, drmVersionPtr, drmGetVersion, (int fd), (fd), NULL)
STUB(LIBDRM, int, drmGetNodeTypeFromFd, (int fd), (fd), -ENOSYS)
STUB(LIBDRM, char *, drmGetRenderDeviceNameFromFd, (int fd), (fd), NULL)
STUB(LIBDRM, int, drmGetDevice, (int fd, drmDevicePtr *device), (fd, device), -ENOSYS)
// clang-format on

// Only a library that loaded can have handed these out.
void drmFreeVersion(drmVersionPtr version) {
    void (*f)(drmVersionPtr) = (void (*)(drmVersionPtr))resolve(LIBDRM, "drmFreeVersion");
    if (f)
        f(version);
}

void drmFreeDevice(drmDevicePtr *device) {
    void (*f)(drmDevicePtr *) = (void (*)(drmDevicePtr *))resolve(LIBDRM, "drmFreeDevice");
    if (f)
        f(device);
}
