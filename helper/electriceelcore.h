#include <stdarg.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>

typedef enum CoreError {
  Ok = 0,
  /**
   * A `String` return slot was NULL, or an input pointer was NULL where a
   * value was required.
   */
  BadArg = 1,
  /**
   * The payload slot (bool/string) could not be written (e.g. an output
   * buffer that is NULL for a value the call must return).
   */
  Internal = 2,
} CoreError;

typedef struct Core Core;

/**
 * Free a string previously returned by any output slot. NULL is a no-op.
 * # Safety
 * `ptr` must be a pointer returned by one of this module's functions, or NULL.
 */
void core_string_free(char *ptr);

/**
 * The build version of the core, stamped from `CARGO_PKG_VERSION` (same git
 * tag as `APP_VERSION` in harbour-electric-eel.pro). Static storage - do not
 * free.
 *
 * # Panics
 *
 * Only if `CARGO_PKG_VERSION` contains an interior NUL byte, which the
 * Cargo.toml version field can't express.
 */
const char *core_version(void);

/**
 * Create the control core.
 *
 * # Arguments
 * - `bin_dir`: directory holding the bundled tesla-session (and, as
 *   fallback, tesla-control/tesla-keygen) binaries.
 * - `state_dir`: writable directory for config.json / `private_key.pem` /
 *   `public_key.pem`.
 * - `session_bin`: path to the bundled `tesla-session` binary; NULL disables
 *   the persistent session (falls back to one-shot binaries).
 * - `ble_backend`: "bluez" or "hci" (spawn flag for tesla-session); NULL is
 *   treated as "hci".
 * - `err_out`: receives a caller-freed error message on failure, may be NULL.
 *
 * # Safety
 * String arguments must be NUL-terminated valid UTF-8; `err_out` must point
 * to writable memory or be NULL.
 */
struct Core *core_new(const char *bin_dir,
                      const char *state_dir,
                      const char *session_bin,
                      const char *ble_backend,
                      char **err_out);

/**
 * # Safety
 * `core` must be a pointer returned by `core_new`, or NULL (no-op). Freed
 * exactly once.
 */
void core_free(struct Core *core);

/**
 * `GetConfig` equivalent: returns the persisted config and key presence.
 *
 * Outputs (all optional, NULL = skip; string outputs are caller-freed):
 * vin, model, `key_name`, `connect_timeout_sec`, `command_timeout_sec`, `has_key`,
 * `public_key_pem`.
 *
 * # Safety
 * `core` must be valid; string output pointers must be writable or NULL.
 */
enum CoreError core_get_status(struct Core *core,
                               char **vin,
                               char **model,
                               char **key_name,
                               int32_t *connect_timeout_sec,
                               int32_t *command_timeout_sec,
                               bool *has_key,
                               char **public_key_pem);

/**
 * `SetConfig` equivalent. On `Ok`, `error_message` receives a caller-freed
 * message only when the payload was accepted-but-failed (`ok=false`, e.g. a
 * validation error); on `BadArg`/`Internal` the message explains the ABI
 * failure.
 *
 * # Safety
 * `core` must be valid; all strings NUL-terminated UTF-8; `ok`/`error_message`
 * writable.
 */
enum CoreError core_set_config(struct Core *core,
                               const char *vin,
                               const char *model,
                               const char *key_name,
                               int32_t connect_timeout_sec,
                               int32_t command_timeout_sec,
                               bool *ok,
                               char **error_message);

/**
 * `GenerateKey` equivalent. On success `ok=true` and `public_key_pem` holds the
 * PEM; on a refused generation `ok=false` and `error_message` explains why.
 * ABI failures are `BadArg`/`Internal`.
 *
 * # Safety
 * `core` must be valid; `force` is an int 0/1; output pointers writable/NULL.
 */
enum CoreError core_generate_key(struct Core *core,
                                 bool force,
                                 bool *ok,
                                 char **public_key_pem,
                                 char **error_message);

/**
 * Pair equivalent. Same shape as `core_generate_key`.
 *
 * # Safety
 * `core` must be valid; output pointers writable/NULL.
 */
enum CoreError core_pair(struct Core *core, bool *ok, char **stdout_out, char **error_message);

/**
 * Starts automatic phone-key presence mode. `active` reports whether the
 * service is running; an inactive result may carry a caller-freed reason.
 *
 * # Safety
 * `core` must be valid; output pointers writable or NULL.
 */
enum CoreError core_start_phone_key(struct Core *core, bool *active, char **error_message);

/**
 * Notifies the core that the device resumed from system suspend.
 *
 * Kills any idle `tesla-session` child whose `org.bluez` system-bus socket
 * is likely stale after the freezer, clears queued stale events, and
 * best-effort restarts phone-key presence if the current config is paired.
 * Idempotent. Never fails except for a NULL handle.
 *
 * # Safety
 * `core` must be a pointer returned by `core_new`, or NULL.
 */
enum CoreError core_handle_resume(struct Core *core);

/**
 * Pops one queued phone-key event without blocking.
 *
 * # Safety
 * `core` must be valid; output pointers writable or NULL. Returned strings
 * must be released with `core_string_free`.
 */
enum CoreError core_poll_phone_key_event(struct Core *core,
                                         bool *has_event,
                                         char **kind,
                                         char **vin,
                                         char **time,
                                         char **error_message);

/**
 * Run a command. `args` is a NULL-terminated array of NUL-terminated UTF-8
 * strings (an empty array = a single NULL element). Results are written to
 * the optional output slots (`ok`, `out_stdout`, `out_stderr`,
 * `out_exit_code`); a *hard* failure (unknown command, busy adapter, refused
 * for policy) is reported through `error_message` (caller-freed) rather than
 * as command output. On an ABI-level failure (bad args, dead core) the
 * function returns `BadArg`/`Internal` and no outputs are valid.
 *
 * # Safety
 * `core` must be valid; `args` must be a NULL-terminated array; string
 * output pointers writable/NULL.
 */
enum CoreError core_run(struct Core *core,
                        const char *cmd,
                        const char *const *args,
                        bool *ok,
                        char **out_stdout,
                        char **out_stderr,
                        int32_t *out_exit_code,
                        char **error_message);

/**
 * Preview a navigation share without sending: parses `text` and reports
 * (kind, value1, value2) = ("gps", lat, lon) or ("address", text, "").
 * A parse failure is `ok=false` + `error_message`, same soft shape as
 * `core_generate_key`.
 *
 * # Safety
 * `core` must be valid; strings NUL-terminated UTF-8; outputs writable/NULL.
 */
enum CoreError core_preview_destination(struct Core *core,
                                        const char *text,
                                        bool *ok,
                                        char **kind,
                                        char **value1,
                                        char **value2,
                                        char **error_message);

/**
 * Send shared text to the car navigation over BLE (session child).
 * Same output shape as `core_pair`.
 *
 * # Safety
 * `core` must be valid; strings NUL-terminated UTF-8; outputs writable/NULL.
 */
enum CoreError core_share_destination(struct Core *core,
                                      const char *text,
                                      bool *ok,
                                      char **stdout_out,
                                      char **error_message);
