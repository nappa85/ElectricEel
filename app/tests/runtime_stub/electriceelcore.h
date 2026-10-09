#pragma once
#include <stdbool.h>

// The three C ABI entry points used by TeslaClient. The test supplies a
// rejecting runtime so admission failures exercise the actual Qt adapter.
struct Runtime;
bool runtime_submit(struct Runtime *runtime, const char *request);
void runtime_observe(struct Runtime *runtime, void (*callback)(void *, const char *), void *context);
const char *core_version(void);
