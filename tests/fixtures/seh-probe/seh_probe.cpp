// A stand-in title for the Linux exception tests (crates/victory-hle/tests/seh_guest.rs),
// written by us: __try/__except/__finally and C++ throw/catch compiled by clang-cl, run by the
// static vcruntime's own __C_specific_handler and __CxxFrameHandler on top of our dispatcher.
// Each probe returns 0, or the number of the check that failed.
//
// Built with /EHa: clang only keeps a __try around a faulting instruction (not a call) when
// asynchronous exceptions are modelled.

extern "C" {
struct Record {  // EXCEPTION_RECORD
    unsigned long code, flags;
    Record* next;
    void* address;
    unsigned long count;
    unsigned __int64 info[15];
};
struct Context {  // CONTEXT, up to Rip
    unsigned char before_integers[0x78];
    unsigned __int64 integer[16];
    unsigned __int64 rip;
};
struct Pointers {
    Record* record;
    Context* context;
};
using Filter = long (*)(Pointers*);

__declspec(dllimport) void RaiseException(unsigned long code, unsigned long flags, unsigned long count,
                                          const unsigned __int64* arguments);
__declspec(dllimport) void* AddVectoredExceptionHandler(unsigned long first, Filter handler);
__declspec(dllimport) unsigned long RemoveVectoredExceptionHandler(void* handle);
__declspec(dllimport) unsigned short RtlCaptureStackBackTrace(unsigned long skip, unsigned long count,
                                                              void** frames, unsigned long* hash);
__declspec(dllimport) Filter SetUnhandledExceptionFilter(Filter filter);

unsigned long __cdecl _exception_code(void);
void* __cdecl _exception_info(void);
bool __cdecl __vcrt_initialize(void);
}

namespace {

constexpr unsigned long kAccessViolation = 0xC0000005, kDivideByZero = 0xC0000094, kBreakpoint = 0x80000003;
constexpr unsigned long kOurs = 0xE0001234;

int* volatile g_low = reinterpret_cast<int*>(0x10);
volatile int g_zero = 0;
volatile int g_sink = 0;

// The order things happen in, as decimal digits.
int g_order = 0;
void step(int digit) { g_order = g_order * 10 + digit; }

struct Guard {  // a local whose destructor an unwind has to run
    int digit;
    explicit Guard(int d) : digit(d) {}
    ~Guard() { step(digit); }
};

__declspec(noinline) void raise_ours(unsigned __int64 a, unsigned __int64 b) {
    const unsigned __int64 args[2] = {a, b};
    RaiseException(kOurs, 0, 2, args);
}

Record g_seen;
long remember(void* info) {
    g_seen = *static_cast<Pointers*>(info)->record;
    return 1;
}

__declspec(noinline) void throw_int(int value) {
    Guard g(3);
    throw value;
}
__declspec(noinline) void level2(int value) {
    Guard g(2);
    throw_int(value);
}
__declspec(noinline) void level1(int value) {
    Guard g(1);
    level2(value);
}

struct Thing {
    int value;
};

long g_vectored_seen = 0;
long on_vectored(Pointers* p) {
    if (p->record->code == kAccessViolation) g_vectored_seen = p->record->code;
    if (p->record->code == kOurs && p->record->info[0] == 99) return -1;  // continue execution
    return 0;
}

long g_filter_code = 0;
long top_level(Pointers* p) {
    g_filter_code = p->record->code;
    return 0;  // EXCEPTION_CONTINUE_SEARCH: the process ends
}

}  // namespace

extern "C" {

__declspec(dllexport) int probe_init() { return __vcrt_initialize() ? 0 : 1; }

// A fault, and a division by zero, caught by __except.
__declspec(dllexport) int seh_faults() {
    __try {
        *g_low = 1;
        return 1;
    } __except (remember(_exception_info())) {
    }
    if (g_seen.code != kAccessViolation || g_seen.info[0] != 1 || g_seen.info[1] != 0x10) return 2;
    __try {
        g_sink = 7 / g_zero;
        return 3;
    } __except (_exception_code() == kDivideByZero) {
    }
    __try {
        __debugbreak();
        return 4;
    } __except (_exception_code() == kBreakpoint) {
    }
    return 0;
}

// RaiseException across a frame, its parameters, and __finally on the way out.
__declspec(dllexport) int seh_raise() {
    g_order = 0;
    __try {
        __try {
            raise_ours(5, 6);
            return 1;
        } __finally {
            step(1);
        }
    } __except (step(2), remember(_exception_info())) {
        step(3);
    }
    if (g_order != 213) return 2;  // the filter runs first, the __finally during the unwind
    if (g_seen.code != kOurs || g_seen.count != 2 || g_seen.info[0] != 5 || g_seen.info[1] != 6) return 3;
    // A filter that continues execution: RaiseException returns.
    int continued = 0;
    __try {
        raise_ours(1, 2);
        continued = 1;
    } __except (-1) {
        return 4;
    }
    if (!continued) return 5;
    // An exception raised inside an __except block is caught further out.
    __try {
        __try {
            raise_ours(1, 0);
        } __except (1) {
            raise_ours(2, 0);
        }
        return 6;
    } __except (remember(_exception_info())) {
    }
    return g_seen.info[0] == 2 ? 0 : 7;
}

// A vectored handler sees a fault before the frame handlers, and can continue a raise.
__declspec(dllexport) int seh_vectored() {
    void* handle = AddVectoredExceptionHandler(1, on_vectored);
    if (!handle) return 1;
    g_vectored_seen = 0;
    int caught = 0;
    __try {
        *g_low = 2;
    } __except (1) {
        caught = 1;
    }
    raise_ours(99, 0);  // continued by the vectored handler
    RemoveVectoredExceptionHandler(handle);
    if (!caught) return 2;
    return g_vectored_seen == kAccessViolation ? 0 : 3;
}

// C++: an int across three frames with destructors, catch by type, rethrow, a throw out of a
// catch block.
__declspec(dllexport) int cpp_throw() {
    g_order = 0;
    volatile int kept = 7;
    try {
        level1(42);
        return 1;
    } catch (int value) {
        step(4);
        if (value != 42) return 2;
    }
    if (g_order != 3214 || kept != 7) return 3;
    g_order = 0;
    try {
        try {
            level1(5);
        } catch (const Thing&) {
            return 4;
        } catch (int) {
            Guard g(5);
            step(4);
            throw;
        }
    } catch (int value) {
        if (value != 5) return 5;
        step(6);
    }
    if (g_order != 321456) return 6;
    try {
        try {
            throw Thing{1};
        } catch (const Thing&) {
            throw Thing{2};
        }
    } catch (const Thing& t) {
        if (t.value != 2) return 7;
    } catch (...) {
        return 8;
    }
    return 0;
}

// The return addresses of three nested frames.
__declspec(noinline) int walk3(void** frames) { return RtlCaptureStackBackTrace(0, 8, frames, nullptr); }
__declspec(noinline) int walk2(void** frames) { return walk3(frames) + 0 * g_sink; }
__declspec(dllexport) int probe_backtrace(void** frames) { return walk2(frames) + 0 * g_sink; }

// Nobody handles it: the top-level filter is asked, then the process ends with the code.
__declspec(dllexport) int seh_unhandled() {
    SetUnhandledExceptionFilter(top_level);
    *g_low = 3;
    return 1;
}
}
