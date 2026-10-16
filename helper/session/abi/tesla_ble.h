#ifndef ELECTRIC_EEL_TESLA_BLE_H
#define ELECTRIC_EEL_TESLA_BLE_H
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* ABI v2. Rust owns policy, queuing, key files and callback context.
 * Input bytes are copied before calls return. Callback bytes are borrowed only
 * during that callback. No Go pointers cross this ABI. Callbacks are serialized
 * per session, run on Go threads, and must not block or re-enter the library.
 * A completion can run before submit returns: register pending state first.
 * Library must remain loaded for the application's lifetime. */
enum ee_status {
    EE_OK = 0, EE_INVALID = 1, EE_BUSY = 2, EE_STOPPING = 3,
    EE_INVALID_HANDLE = 4, EE_FAILED = 5, EE_CANCELLED = 6,
    EE_TIMED_OUT = 7, EE_INTERNAL_BUG = 8, EE_UNCERTAIN = 9,
    EE_NOT_CONNECTED = 10, EE_BLUETOOTH_OFF = 11, EE_AUTHENTICATION_FAILED = 12
};
enum ee_operation {
    EE_CONNECT = 1, EE_DISCONNECT = 2, EE_BODY_CONTROLLER_STATE = 3,
    EE_NAVIGATE_GPS = 4, EE_NAVIGATE_ADDRESS = 5, EE_WATCH_START = 6,
    EE_WATCH_STOP = 7, EE_PASSIVE_ENTRY = 8, EE_ENROLLMENT_STATUS = 9,
    EE_VEHICLE_COMMAND = 10, EE_PUBLIC_KEY = 11
};
enum ee_domain { EE_VCSEC = 1, EE_INFOTAINMENT = 2 };
/* Stable command IDs. Typed parameter layouts are documented by commandSchema
 * in library/commands.go and parsed by Rust protocol_commands.rs. */
enum ee_command {
    EE_CMD_VALET_ON=1, EE_CMD_VALET_OFF, EE_CMD_UNLOCK, EE_CMD_LOCK,
    EE_CMD_DRIVE, EE_CMD_CLIMATE_ON, EE_CMD_CLIMATE_OFF, EE_CMD_CLIMATE_TEMP,
    EE_CMD_ADD_KEY, EE_CMD_ADD_KEY_REQUEST, EE_CMD_REMOVE_KEY, EE_CMD_RENAME_KEY,
    EE_CMD_LIST_KEYS, EE_CMD_HONK, EE_CMD_PING, EE_CMD_FLASH_LIGHTS,
    EE_CMD_ACCESSORY_POWER, EE_CMD_LOW_POWER, EE_CMD_CHARGE_LIMIT, EE_CMD_CHARGE_AMPS,
    EE_CMD_CHARGE_START, EE_CMD_CHARGE_STOP, EE_CMD_CHARGE_SCHEDULE,
    EE_CMD_CHARGE_SCHEDULE_CANCEL, EE_CMD_CHARGE_SCHEDULE_ADD,
    EE_CMD_CHARGE_SCHEDULE_REMOVE, EE_CMD_VOLUME, EE_CMD_VOLUME_UP,
    EE_CMD_VOLUME_DOWN, EE_CMD_NEXT_FAVORITE, EE_CMD_NEXT_TRACK,
    EE_CMD_PREVIOUS_TRACK, EE_CMD_PREVIOUS_FAVORITE, EE_CMD_PLAYBACK,
    EE_CMD_SOFTWARE_START, EE_CMD_SOFTWARE_CANCEL, EE_CMD_SENTRY, EE_CMD_WAKE,
    EE_CMD_TONNEAU_OPEN, EE_CMD_TONNEAU_CLOSE, EE_CMD_TONNEAU_STOP,
    EE_CMD_TRUNK_OPEN, EE_CMD_TRUNK_MOVE, EE_CMD_TRUNK_CLOSE, EE_CMD_FRUNK_OPEN,
    EE_CMD_CHARGE_PORT_OPEN, EE_CMD_CHARGE_PORT_CLOSE, EE_CMD_AUTOSECURE,
    EE_CMD_SESSION_INFO, EE_CMD_SEAT_HEATER, EE_CMD_STEERING_HEATER,
    EE_CMD_AUTO_SEAT, EE_CMD_WINDOWS_VENT, EE_CMD_WINDOWS_CLOSE,
    EE_CMD_BODY_STATE, EE_CMD_GUEST_ON, EE_CMD_GUEST_OFF, EE_CMD_ERASE_GUEST,
    EE_CMD_PRECONDITION_ADD, EE_CMD_PRECONDITION_REMOVE, EE_CMD_PRODUCT_INFO,
    EE_CMD_STATE, EE_CMD_VEHICLE_NAME, EE_CMD_SET_VEHICLE_NAME
};
enum ee_observation {
    EE_ADVERTISEMENT=1, EE_DISCONNECTED, EE_AUTHENTICATED, EE_AUTH_FAILED,
    EE_WATCH_FAILED, EE_LOG
};
enum ee_notification { EE_COMPLETE = 1, EE_DIAGNOSTIC = 2, EE_STOPPED = 3, EE_OBSERVATION = 4 };

typedef struct ee_bytes { const uint8_t *data; uint64_t len; } ee_bytes;
typedef struct ee_config {
    uint32_t size;
    uint32_t version;
    ee_bytes vin;
    ee_bytes adapter;
    ee_bytes private_scalar; /* 32-byte, big-endian P-256 private scalar */
} ee_config;
/* Typed command parameters: 0 omitted, 1 integer/enum/bool, 2 double, 3 bytes.
 * Numeric values are already parsed/normalized by Rust. */
typedef struct ee_value {
    uint32_t kind;
    uint32_t reserved;
    uint64_t integer;
    double number;
    ee_bytes bytes;
} ee_value;
typedef struct ee_request {
    uint32_t size;
    uint32_t operation;
    uint64_t id; /* nonzero and strictly increasing within a session */
    uint64_t timeout_ms;
    uint32_t domain; /* Connect only */
    uint32_t reserved; /* must be zero */
    double latitude;
    double longitude;
    ee_bytes text;
    uint64_t target; /* opaque fresh discovery target; zero means explicit scan */
    uint32_t command;
    uint32_t count;
    const ee_value *values;
} ee_request;
typedef struct ee_message {
    uint32_t size;
    uint32_t kind;
    uint64_t session;
    uint64_t request;
    uint32_t status;
    uint32_t reserved;
    ee_bytes payload; /* VCSEC status: protobuf-derived JSON, defaults emitted */
    ee_bytes error;
    uint32_t event;
    int32_t rssi;
    uint64_t target;
    uint32_t value;
    uint32_t padding;
} ee_message;
typedef void (*ee_callback)(void *context, const ee_message *message);

uint32_t ee_abi_version(void);
uint32_t ee_session_create(ee_config *config, ee_callback callback,
                           void *context, uint64_t *handle);
uint32_t ee_session_submit(uint64_t handle, ee_request *request);
uint32_t ee_session_cancel(uint64_t handle, uint64_t request);
uint32_t ee_session_stop(uint64_t handle);
/* destroy requires stop first. It waits for backend teardown AND return from
 * the stopped callback before releasing the handle. Never call from a callback
 * or UI thread. After it returns, callback context may be released. A backend
 * that cannot stop intentionally blocks destruction rather than faking safety. */
uint32_t ee_session_destroy(uint64_t handle);

#ifdef __cplusplus
}
#endif
#endif
