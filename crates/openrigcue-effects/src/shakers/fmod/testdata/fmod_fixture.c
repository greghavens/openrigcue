/*
 * A compiled stand-in for the FMOD 2.02 Core and Studio libraries, built by the
 * `compiled_fixture_*` tests as libfmod.so.13 (-DFIXTURE_CORE) and libfmodstudio.so.13
 * (-DFIXTURE_STUDIO, linked against the core library like the real one). It is not FMOD:
 * it implements only the calls OpenRigCue binds, with the call-time checks and buffering
 * of FMOD 2.02.10:
 *
 * - setParameterByName: a null name or a name of 128
 *   bytes or more is FMOD_ERR_INVALID_PARAM, an uninitialized system
 *   FMOD_ERR_STUDIO_UNINITIALIZED, an infinite or NaN value FMOD_ERR_INVALID_FLOAT, an
 *   unknown name FMOD_ERR_EVENT_NOTFOUND; otherwise the command is appended to
 *   the system's buffer.
 * - setPaused writes the instance's API-side paused byte, then
 *   buffers; getPaused reads that byte.
 * - Studio::System::update runs the buffer, then the core update (one block
 *   with the non-real-time output).
 *
 * The mix is synthetic: each started, unpaused instance adds its fl, fr, ce, sub, sl, sr
 * parameters to the six channels of every frame of the block.
 */
#include <math.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

typedef int FMOD_RESULT;
enum {
    FMOD_OK = 0,
    FMOD_ERR_FORMAT = 19,
    FMOD_ERR_HEADER_MISMATCH = 20,
    FMOD_ERR_INVALID_FLOAT = 29,
    FMOD_ERR_INVALID_HANDLE = 30,
    FMOD_ERR_INVALID_PARAM = 31,
    FMOD_ERR_EVENT_NOTFOUND = 74,
    FMOD_ERR_STUDIO_UNINITIALIZED = 75,
};
#define VERSION 0x00020210u
#define NRT 4
#define CHANNELS 6

typedef struct DspState DspState;
typedef FMOD_RESULT (*ReadCallback)(DspState *, float *, float *, unsigned int, int, int *);
typedef FMOD_RESULT (*GetUserData)(DspState *, void **);

/* fmod_dsp.h (2.02) layouts. */
typedef struct {
    void *alloc, *realloc, *free, *getsamplerate, *getblocksize, *dft, *pan, *getspeakermode, *getclock,
        *getlistenerattributes, *log;
    GetUserData getuserdata;
} DspStateFunctions;
struct DspState {
    void *instance;
    void *plugindata;
    unsigned int channelmask;
    int source_speakermode;
    float *sidechaindata;
    int sidechainchannels;
    const DspStateFunctions *functions;
    int systemobject;
};
typedef struct {
    unsigned int pluginsdkversion;
    char name[32];
    unsigned int version;
    int numinputbuffers, numoutputbuffers;
    void *create, *release, *reset;
    ReadCallback read;
    void *process, *setposition;
    int numparameters;
    void *paramdesc, *setparameterfloat, *setparameterint, *setparameterbool, *setparameterdata,
        *getparameterfloat, *getparameterint, *getparameterbool, *getparameterdata, *shouldiprocess;
    void *userdata;
    void *sys_register, *sys_deregister, *sys_mix;
} DspDescription;


/* The executed (mixed) state of the two instances lives in the core library, which mixes. */
typedef struct {
    int started;
    int paused;
    float params[9];
} Executed;

#ifdef FIXTURE_CORE
static const char *const SPEAKERS[CHANNELS] = {"fl", "fr", "ce", "sub", "sl", "sr"};
static const char *const KNOWN[9] = {"rpm", "load", "fl", "fr", "ce", "sub", "sl", "sr", "pitch"};

typedef struct {
    int output, rate, mode, raw;
    unsigned int block;
    const DspDescription *dsp;
    int dsp_added;
    Executed executed[2];
} CoreSystem;

static int known_index(const char *name) {
    for (int i = 0; i < 9; i++)
        if (strcmp(name, KNOWN[i]) == 0) return i;
    return -1;
}

static FMOD_RESULT fixture_get_user_data(DspState *state, void **out) {
    *out = state->plugindata;
    return FMOD_OK;
}

FMOD_RESULT FMOD_System_Create(void **out, unsigned int header) {
    if ((header >> 8) != (VERSION >> 8)) return FMOD_ERR_HEADER_MISMATCH;
    CoreSystem *s = calloc(1, sizeof *s);
    s->rate = 48000;
    s->mode = 3;
    s->block = 1024;
    *out = s;
    return FMOD_OK;
}
FMOD_RESULT FMOD_System_GetVersion(void *s, unsigned int *v) {
    (void)s;
    *v = VERSION;
    return FMOD_OK;
}
FMOD_RESULT FMOD_System_Close(void *s) {
    (void)s;
    return FMOD_OK;
}
FMOD_RESULT FMOD_System_Release(void *s) {
    free(s);
    return FMOD_OK;
}
FMOD_RESULT FMOD_System_SetOutput(void *s, int output) {
    ((CoreSystem *)s)->output = output;
    return FMOD_OK;
}
FMOD_RESULT FMOD_System_GetSoftwareFormat(void *p, int *rate, int *mode, int *raw) {
    CoreSystem *s = p;
    *rate = s->rate;
    *mode = s->mode;
    *raw = s->raw;
    return FMOD_OK;
}
FMOD_RESULT FMOD_System_SetSoftwareFormat(void *p, int rate, int mode, int raw) {
    CoreSystem *s = p;
    s->rate = rate;
    s->mode = mode;
    s->raw = raw;
    return FMOD_OK;
}
FMOD_RESULT FMOD_System_SetDSPBufferSize(void *p, unsigned int len, int num) {
    (void)num;
    ((CoreSystem *)p)->block = len;
    return FMOD_OK;
}
FMOD_RESULT FMOD_System_CreateDSP(void *p, const DspDescription *desc, void **out) {
    if (desc->pluginsdkversion != 110) return FMOD_ERR_HEADER_MISMATCH;
    ((CoreSystem *)p)->dsp = desc;
    *out = (void *)desc;
    return FMOD_OK;
}
FMOD_RESULT FMOD_System_GetMasterChannelGroup(void *p, void **out) {
    *out = p;
    return FMOD_OK;
}
FMOD_RESULT FMOD_ChannelGroup_AddDSP(void *group, int index, void *dsp) {
    (void)dsp;
    if (index != -1) return FMOD_ERR_INVALID_PARAM;
    ((CoreSystem *)group)->dsp_added = 1;
    return FMOD_OK;
}
FMOD_RESULT FMOD_ChannelGroup_RemoveDSP(void *group, void *dsp) {
    (void)dsp;
    ((CoreSystem *)group)->dsp_added = 0;
    return FMOD_OK;
}
FMOD_RESULT FMOD_DSP_Release(void *dsp) {
    (void)dsp;
    return FMOD_OK;
}

/* Fixture-only bridge for the Studio library: the executed state the mix reads. */
Executed *fixture_core_executed(void *p) { return ((CoreSystem *)p)->executed; }
int fixture_known_index(const char *name) { return known_index(name); }

/* The non-real-time output mixes one block per update, through the DSP chain's head. */
FMOD_RESULT FMOD_System_Update(void *p) {
    CoreSystem *s = p;
    if (s->output != NRT || !s->dsp || !s->dsp_added || !s->dsp->read) return FMOD_OK;
    float frame[CHANNELS] = {0};
    for (int i = 0; i < 2; i++) {
        if (!s->executed[i].started || s->executed[i].paused) continue;
        for (int c = 0; c < CHANNELS; c++) frame[c] += s->executed[i].params[known_index(SPEAKERS[c])];
    }
    size_t n = (size_t)s->block * CHANNELS;
    float *in = malloc(n * sizeof *in), *out = malloc(n * sizeof *out);
    for (size_t k = 0; k < n; k++) {
        in[k] = frame[k % CHANNELS];
        out[k] = 0;
    }
    DspStateFunctions functions = {0};
    functions.getuserdata = fixture_get_user_data;
    DspState state = {0};
    state.plugindata = s->dsp->userdata;
    state.source_speakermode = s->mode;
    state.functions = &functions;
    int out_channels = CHANNELS;
    FMOD_RESULT r = s->dsp->read(&state, in, out, s->block, CHANNELS, &out_channels);
    int passed = memcmp(in, out, n * sizeof *in) == 0;
    free(in);
    free(out);
    return r != FMOD_OK ? r : (passed ? FMOD_OK : FMOD_ERR_FORMAT);
}
#endif

#ifdef FIXTURE_STUDIO
FMOD_RESULT FMOD_System_Create(void **, unsigned int);
FMOD_RESULT FMOD_System_Release(void *);
FMOD_RESULT FMOD_System_Update(void *);
Executed *fixture_core_executed(void *);
int fixture_known_index(const char *);

typedef struct {
    int instance;
    int kind; /* 0 paused, 1 parameter */
    int index;
    float value;
} Command;
typedef struct {
    void *core;
    int initialized;
    int banks;
    int created[2];
    int api_paused[2];
    Command buffer[256];
    int queued;
} StudioSystem;

/* Handles: descriptions and instances point into the system; their low bit pattern names
 * the event (0 = Car Engine 1, 1 = Car Engine 2). */
typedef struct {
    StudioSystem *system;
    int event;
} Object;
static Object descriptions[2], instances[2];

static const char *const EVENTS[2] = {"event:/Vehicles/Car Engine 1", "event:/Vehicles/Car Engine 2"};

static FMOD_RESULT append(StudioSystem *s, Command c) {
    if (s->queued == 256) return FMOD_ERR_INVALID_PARAM;
    s->buffer[s->queued++] = c;
    return FMOD_OK;
}

FMOD_RESULT FMOD_Studio_System_Create(void **out, unsigned int header) {
    if ((header >> 8) != (VERSION >> 8)) return FMOD_ERR_HEADER_MISMATCH;
    StudioSystem *s = calloc(1, sizeof *s);
    FMOD_RESULT r = FMOD_System_Create(&s->core, header);
    if (r != FMOD_OK) {
        free(s);
        return r;
    }
    *out = s;
    return FMOD_OK;
}
FMOD_RESULT FMOD_Studio_System_GetCoreSystem(void *p, void **out) {
    *out = ((StudioSystem *)p)->core;
    return FMOD_OK;
}
FMOD_RESULT FMOD_Studio_System_Initialize(void *p, int max, unsigned int studio_flags, unsigned int flags,
                                          void *extra) {
    (void)max;
    (void)studio_flags;
    (void)flags;
    (void)extra;
    ((StudioSystem *)p)->initialized = 1;
    return FMOD_OK;
}
FMOD_RESULT FMOD_Studio_System_LoadBankMemory(void *p, const char *data, int len, int mode, unsigned int flags,
                                              void **bank) {
    (void)data;
    (void)flags;
    StudioSystem *s = p;
    if (!s->initialized) return FMOD_ERR_STUDIO_UNINITIALIZED;
    if (len <= 0 || mode != 0) return FMOD_ERR_INVALID_PARAM;
    s->banks++;
    *bank = s;
    return FMOD_OK;
}
FMOD_RESULT FMOD_Studio_System_GetEvent(void *p, const char *path, void **out) {
    StudioSystem *s = p;
    if (!s->initialized) return FMOD_ERR_STUDIO_UNINITIALIZED;
    for (int i = 0; i < 2; i++) {
        if (s->banks == 3 && strcmp(path, EVENTS[i]) == 0) {
            descriptions[i] = (Object){s, i};
            *out = &descriptions[i];
            return FMOD_OK;
        }
    }
    return FMOD_ERR_EVENT_NOTFOUND;
}
FMOD_RESULT FMOD_Studio_EventDescription_CreateInstance(void *d, void **out) {
    Object *desc = d;
    instances[desc->event] = *desc;
    desc->system->created[desc->event] = 1;
    *out = &instances[desc->event];
    return FMOD_OK;
}
FMOD_RESULT FMOD_Studio_EventInstance_Start(void *i) {
    Object *inst = i;
    fixture_core_executed(inst->system->core)[inst->event].started = 1;
    return FMOD_OK;
}
FMOD_RESULT FMOD_Studio_EventInstance_SetPaused(void *i, int paused) {
    Object *inst = i;
    if (!inst->system->initialized) return FMOD_ERR_STUDIO_UNINITIALIZED;
    inst->system->api_paused[inst->event] = paused != 0;
    return append(inst->system, (Command){inst->event, 0, 0, paused != 0});
}
FMOD_RESULT FMOD_Studio_EventInstance_GetPaused(void *i, int *paused) {
    Object *inst = i;
    if (!paused) return FMOD_ERR_INVALID_PARAM;
    *paused = inst->system->api_paused[inst->event];
    return FMOD_OK;
}
FMOD_RESULT FMOD_Studio_EventInstance_SetParameterByName(void *i, const char *name, float value, int ignore) {
    (void)ignore;
    Object *inst = i;
    if (!name || strlen(name) >= 0x80) return FMOD_ERR_INVALID_PARAM;
    if (!inst->system->initialized) return FMOD_ERR_STUDIO_UNINITIALIZED;
    uint32_t bits;
    memcpy(&bits, &value, sizeof bits);
    if ((bits & 0x7f800000u) == 0x7f800000u) return FMOD_ERR_INVALID_FLOAT;
    int index = fixture_known_index(name);
    if (index < 0) return FMOD_ERR_EVENT_NOTFOUND;
    return append(inst->system, (Command){inst->event, 1, index, value});
}
FMOD_RESULT FMOD_Studio_System_Update(void *p) {
    StudioSystem *s = p;
    if (!s->initialized) return FMOD_ERR_STUDIO_UNINITIALIZED;
    Executed *executed = fixture_core_executed(s->core);
    for (int k = 0; k < s->queued; k++) {
        Command c = s->buffer[k];
        if (c.kind == 0)
            executed[c.instance].paused = c.value != 0;
        else
            executed[c.instance].params[c.index] = c.value;
    }
    s->queued = 0;
    return FMOD_System_Update(s->core);
}
FMOD_RESULT FMOD_Studio_System_Release(void *p) {
    StudioSystem *s = p;
    FMOD_System_Release(s->core);
    free(s);
    return FMOD_OK;
}
#endif
