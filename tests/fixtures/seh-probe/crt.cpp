// The few C library functions the static vcruntime's exception code needs, so the probe links
// without the UCRT. Ordinary test code.
typedef unsigned long long size_t;

extern "C" {
__declspec(noreturn) void __fastfail(unsigned int);
void abort() { __fastfail(7); }
void terminate() { __fastfail(7); }

static char g_heap[1 << 16];
static size_t g_used;
void* malloc(size_t n) {
    n = (n + 15) & ~15ull;
    if (g_used + n > sizeof g_heap) return nullptr;
    void* p = g_heap + g_used;
    g_used += n;
    return p;
}
void* calloc(size_t a, size_t b) {
    char* p = static_cast<char*>(malloc(a * b));
    for (size_t i = 0; p && i < a * b; i++) p[i] = 0;
    return p;
}
void free(void*) {}
size_t strlen(const char* s) {
    size_t n = 0;
    while (s[n]) n++;
    return n;
}
int strcmp(const char* a, const char* b) {
    while (*a && *a == *b) a++, b++;
    return (unsigned char)*a - (unsigned char)*b;
}
int strncmp(const char* a, const char* b, size_t n) {
    for (; n; n--, a++, b++) {
        if (*a != *b) return (unsigned char)*a - (unsigned char)*b;
        if (!*a) break;
    }
    return 0;
}
int wcsncmp(const wchar_t* a, const wchar_t* b, size_t n) {
    for (; n; n--, a++, b++) {
        if (*a != *b) return *a - *b;
        if (!*a) break;
    }
    return 0;
}
int strcpy_s(char* d, size_t n, const char* s) {
    size_t i = 0;
    for (; i + 1 < n && s[i]; i++) d[i] = s[i];
    d[i] = 0;
    return 0;
}
long atol(const char*) { return 0; }
int __stdio_common_vsprintf(unsigned long long, char*, size_t, const char*, void*, char*) { return -1; }
int __stdio_common_vsprintf_s(unsigned long long, char*, size_t, const char*, void*, char*) { return -1; }
}
