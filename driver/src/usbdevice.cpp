// Virtual USB device (the "Deck") definition + plug-in.
//
// What this file owns:
//   - USB device, configuration, interface, HID, and endpoint descriptors
//     mirroring the real Steam Deck controller (VID 0x28de PID 0x1205).
//   - HID report descriptors for the three HID interfaces (kbd, mouse,
//     vendor-defined gamepad). Returned from EP0 in response to
//     GET_DESCRIPTOR(REPORT) class requests.
//   - String descriptors (manufacturer, product, serial).
//   - UdecxUsbDeviceCreate + descriptor registration calls.
//   - Endpoint queues — interrupt-IN for input reports (x4), bulk-IN/OUT
//     for the (inert) CDC Data interface, plus EP0 control.
//   - Default control-endpoint (EP0) handler that services the HID class
//     request GET_DESCRIPTOR(REPORT) for IF0 / IF1 / IF2, plus the no-op
//     class requests the HID class driver issues during init (SET_IDLE,
//     SET_PROTOCOL, GET_IDLE).
//   - UdecxUsbDevicePlugIn to make the device visible to Windows.
//
// API reference: microsoft/UDE  UDEMbimClientSample/{usbdevice,io}.cpp

#include "common.h"
#include <usbspec.h>

// ===========================================================================
// USB / HID descriptors.
//
// All bytes here are sourced from real Steam Deck hardware via three public
// projects (see memory: deck_descriptor_sources.md):
//
//   - NeroReflex/ROGueENEMY  References/SteamDeck/virt_deck.c
//       Provides the device + configuration descriptor blob (168 bytes).
//   - KWottrich/ally-steam-controller  reference/steam-deck-report-descriptor.txt
//       Provides cross-checked `lsusb -v` decode for IF0 (kbd) and IF1 (mouse)
//       HID report descriptors.
//   - streetpea/chiaki-ng  steamdeck_native/hid_info/hid_report.txt
//       Provides the IF2 (gamepad vendor-HID) report descriptor (29 bytes).
//
// One known discrepancy: older lsusb dumps report IF2's wDescriptorLength as
// 25 bytes; chiaki-ng's modern dump returns 29 bytes. We register 29 bytes
// (0x001D) and patch the config descriptor to match. Steam Input fingerprints
// VID/PID + descriptor structure, so a 4-byte off-by-one here matters.
// ===========================================================================

#define DECK_VID 0x28DE
#define DECK_PID 0x1205

// String indices for the device descriptor.
#define DECK_STR_MANUFACTURER   1
#define DECK_STR_PRODUCT        2
#define DECK_STR_SERIAL         3

#define DECK_LANG_ID_EN_US      0x0409

// ---------------------------------------------------------------------------
// Device descriptor (18 bytes).
// ---------------------------------------------------------------------------
static const UCHAR g_DeviceDescriptor[] = {
    0x12,                       // bLength
    0x01,                       // bDescriptorType = DEVICE
    0x00, 0x02,                 // bcdUSB = 2.00
    0x00,                       // bDeviceClass (composite)
    0x00,                       // bDeviceSubClass
    0x00,                       // bDeviceProtocol
    0x40,                       // bMaxPacketSize0 = 64
    (DECK_VID & 0xFF), (DECK_VID >> 8),
    (DECK_PID & 0xFF), (DECK_PID >> 8),
    0x00, 0x02,                 // bcdDevice = 2.00
    DECK_STR_MANUFACTURER,
    DECK_STR_PRODUCT,
    DECK_STR_SERIAL,
    0x01,                       // bNumConfigurations
};
static_assert(sizeof(g_DeviceDescriptor) == 18, "Device descriptor must be 18 bytes");

// ---------------------------------------------------------------------------
// Configuration descriptor blob (150 bytes total = wTotalLength = 0x0096).
// Five interfaces — see file header for the per-IF breakdown. Valve quirks
// (CDC Call Management bDataInterface=2 instead of 4) mirrored verbatim.
// ---------------------------------------------------------------------------
static const UCHAR g_ConfigDescriptor[] = {
    // ------- CONFIGURATION (9 bytes) -------
    0x09, 0x02, 0x96, 0x00, 0x05, 0x01, 0x00, 0x80, 0xFA,

    // ------- INTERFACE 0: HID Boot Keyboard (25 bytes) -------
    0x09, 0x04, 0x00, 0x00, 0x01, 0x03, 0x01, 0x01, 0x00,
    0x09, 0x21, 0x11, 0x01, 0x21, 0x01, 0x22, 0x27, 0x00,   // wDescLen = 39
    0x07, 0x05, 0x82, 0x03, 0x08, 0x00, 0x01,               // EP 0x82 IN

    // ------- INTERFACE 1: HID Mouse (25 bytes) -------
    0x09, 0x04, 0x01, 0x00, 0x01, 0x03, 0x00, 0x02, 0x00,
    0x09, 0x21, 0x11, 0x01, 0x00, 0x01, 0x22, 0x41, 0x00,   // wDescLen = 65
    0x07, 0x05, 0x81, 0x03, 0x08, 0x00, 0x01,               // EP 0x81 IN

    // ------- INTERFACE 2: HID Vendor (gamepad) (25 bytes) -------
    0x09, 0x04, 0x02, 0x00, 0x01, 0x03, 0x00, 0x00, 0x00,
    0x09, 0x21, 0x11, 0x01, 0x00, 0x01, 0x22, 0x1D, 0x00,   // wDescLen = 29 (modern firmware)
    0x07, 0x05, 0x83, 0x03, 0x40, 0x00, 0x01,               // EP 0x83 IN

    // ------- IAD for CDC ACM (8 bytes) -------
    0x08, 0x0B, 0x03, 0x02, 0x02, 0x02, 0x01, 0x00,

    // ------- INTERFACE 3: CDC ACM control (35 bytes) -------
    0x09, 0x04, 0x03, 0x00, 0x01, 0x02, 0x02, 0x01, 0x00,
    0x05, 0x24, 0x00, 0x10, 0x01,
    0x05, 0x24, 0x01, 0x01, 0x02,                            // bDataIface=2 (Valve quirk)
    0x04, 0x24, 0x02, 0x02,
    0x05, 0x24, 0x06, 0x03, 0x04,
    0x07, 0x05, 0x84, 0x03, 0x40, 0x00, 0xFF,               // EP 0x84 IN

    // ------- INTERFACE 4: CDC Data (23 bytes) -------
    0x09, 0x04, 0x04, 0x00, 0x02, 0x0A, 0x00, 0x00, 0x00,
    0x07, 0x05, 0x85, 0x02, 0x40, 0x00, 0x00,               // EP 0x85 IN
    0x07, 0x05, 0x05, 0x02, 0x40, 0x00, 0x00,               // EP 0x05 OUT
};
static_assert(sizeof(g_ConfigDescriptor) == 0x96,
              "Configuration descriptor must be 150 bytes");

// ---------------------------------------------------------------------------
// HID Report descriptor for IF2 (gamepad — load-bearing). 29 bytes.
// chiaki-ng. Vendor page 0xFFFF; one 64-byte Input + Output + Feature.
// ---------------------------------------------------------------------------
static const UCHAR g_If2HidReportDescriptor[] = {
    0x06, 0xFF, 0xFF, 0x09, 0x01, 0xA1, 0x01, 0x15, 0x00, 0x26, 0xFF, 0x00,
    0x75, 0x08, 0x95, 0x40, 0x09, 0x01, 0x81, 0x02, 0x09, 0x01, 0x91, 0x02,
    0x09, 0x01, 0xB1, 0x02, 0xC0,
};
static_assert(sizeof(g_If2HidReportDescriptor) == 29, "IF2 report descriptor wrong size");

// ---------------------------------------------------------------------------
// HID Report descriptor for IF0 (boot kbd). 39 bytes. HID 1.11 §B.1.
// ---------------------------------------------------------------------------
static const UCHAR g_If0HidReportDescriptor[] = {
    0x05, 0x01, 0x09, 0x06, 0xA1, 0x01, 0x05, 0x07, 0x19, 0xE0, 0x29, 0xE7,
    0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x08, 0x81, 0x02, 0x81, 0x01,
    0x19, 0x00, 0x29, 0x65, 0x15, 0x00, 0x25, 0x65, 0x75, 0x08, 0x95, 0x06,
    0x81, 0x00, 0xC0,
};
static_assert(sizeof(g_If0HidReportDescriptor) == 39, "IF0 report descriptor wrong size");

// ---------------------------------------------------------------------------
// HID Report descriptor for IF1 (mouse). 65 bytes. From ally-steam decode.
// ---------------------------------------------------------------------------
static const UCHAR g_If1HidReportDescriptor[] = {
    0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x09, 0x01, 0xA1, 0x00, 0x05, 0x09,
    0x19, 0x01, 0x29, 0x02, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x02,
    0x81, 0x02, 0x75, 0x06, 0x95, 0x01, 0x81, 0x01, 0x05, 0x01, 0x09, 0x30,
    0x09, 0x31, 0x15, 0x81, 0x25, 0x7F, 0x75, 0x08, 0x95, 0x02, 0x81, 0x06,
    0x95, 0x01, 0x09, 0x38, 0x81, 0x06, 0x05, 0x0C, 0x0A, 0x38, 0x02, 0x95,
    0x01, 0x81, 0x06, 0xC0, 0xC0,
};
static_assert(sizeof(g_If1HidReportDescriptor) == 65, "IF1 report descriptor wrong size");

// ---------------------------------------------------------------------------
// Language-ID descriptor (string index 0). 4 bytes — header + en-US lang ID.
// ---------------------------------------------------------------------------
static const UCHAR g_LanguageDescriptor[] = {
    0x04,                       // bLength
    0x03,                       // bDescriptorType = STRING
    (DECK_LANG_ID_EN_US & 0xFF), (DECK_LANG_ID_EN_US >> 8),
};

// ---------------------------------------------------------------------------
// String descriptors. UdecxUsbDeviceInitAddStringDescriptor takes a
// PCUNICODE_STRING and the index + lang ID, builds the wire-format descriptor
// itself.
// ---------------------------------------------------------------------------
static DECLARE_CONST_UNICODE_STRING(g_ManufacturerString, L"Valve Software");
static DECLARE_CONST_UNICODE_STRING(g_ProductString,      L"Steam Deck Controller");
static DECLARE_CONST_UNICODE_STRING(g_SerialString,       L"MEDA00000001");

// ===========================================================================
// State / endpoint callbacks. All return STATUS_SUCCESS — we don't manage
// power transitions or function suspend/wake yet.
// ===========================================================================

static EVT_UDECX_USB_DEVICE_D0_ENTRY                      EvtUsbDeviceLinkPowerEntry;
static EVT_UDECX_USB_DEVICE_D0_EXIT                       EvtUsbDeviceLinkPowerExit;
static EVT_UDECX_USB_DEVICE_SET_FUNCTION_SUSPEND_AND_WAKE EvtUsbDeviceSetFunctionSuspendAndWake;
static EVT_UDECX_USB_ENDPOINT_RESET                       EvtUsbEndpointReset;

NTSTATUS EvtUsbDeviceLinkPowerEntry(_In_ WDFDEVICE, _In_ UDECXUSBDEVICE)
{ return STATUS_SUCCESS; }

NTSTATUS EvtUsbDeviceLinkPowerExit(_In_ WDFDEVICE, _In_ UDECXUSBDEVICE,
                                   _In_ UDECX_USB_DEVICE_WAKE_SETTING)
{ return STATUS_SUCCESS; }

NTSTATUS EvtUsbDeviceSetFunctionSuspendAndWake(_In_ WDFDEVICE, _In_ UDECXUSBDEVICE,
                                               _In_ ULONG, _In_ UDECX_USB_DEVICE_FUNCTION_POWER)
{ return STATUS_SUCCESS; }

VOID EvtUsbEndpointReset(_In_ UDECXUSBENDPOINT, _In_ WDFREQUEST Request)
{ WdfRequestComplete(Request, STATUS_SUCCESS); }

// ===========================================================================
// Control-endpoint (EP0) URB handler.
//
// UDE auto-services standard DEVICE-recipient GET_DESCRIPTOR (device, config,
// string, BOS) using the descriptors we registered. Everything else lands
// here, which for our device means:
//
//   1. Standard GET_DESCRIPTOR with INTERFACE recipient — the host's HID
//      class driver fetches our HID Report descriptor (type 0x22). Critical
//      for the device to function as HID.
//
//   2. Class-specific HID requests (SET_IDLE 0x0A, SET_PROTOCOL 0x0B,
//      GET_IDLE 0x02, GET_PROTOCOL 0x03, GET_REPORT 0x01, SET_REPORT 0x09).
//      The HID class driver fires SET_IDLE during init and waits for it
//      to complete; if we don't respond, the device hangs in init.
//
// Real Decks expose haptics / rumble via SET_REPORT(FEATURE) on IF2. That
// path eventually connects to PendedOutputQueue (rumble-back-to-Deck) but
// for now we just acknowledge to keep the host happy.
// ===========================================================================

#define HID_DESCRIPTOR_TYPE_HID         0x21
#define HID_DESCRIPTOR_TYPE_REPORT      0x22

#define HID_REQUEST_GET_REPORT          0x01
#define HID_REQUEST_GET_IDLE            0x02
#define HID_REQUEST_GET_PROTOCOL        0x03
#define HID_REQUEST_SET_REPORT          0x09
#define HID_REQUEST_SET_IDLE            0x0A
#define HID_REQUEST_SET_PROTOCOL        0x0B

#define HID_REPORT_TYPE_INPUT           0x01
#define HID_REPORT_TYPE_OUTPUT          0x02
#define HID_REPORT_TYPE_FEATURE         0x03

// ---------------------------------------------------------------------------
// Steam Controller feature-report message IDs (subset we care about).
// Source: tools/reference/hid-steam.c (Linux kernel, kernel-sourced) and
// libsdl-org/SDL src/joystick/hidapi/steam/controller_constants.h.
// ---------------------------------------------------------------------------
#define DECK_MSG_CLEAR_DIGITAL_MAPPINGS   0x81
#define DECK_MSG_GET_ATTRIBUTES_VALUES    0x83
#define DECK_MSG_SET_DEFAULT_DIGITAL_MAPPINGS 0x85
#define DECK_MSG_SET_SETTINGS_VALUES      0x87
#define DECK_MSG_LOAD_DEFAULT_SETTINGS    0x8E
#define DECK_MSG_TRIGGER_HAPTIC_PULSE     0x8F
#define DECK_MSG_GET_DEVICE_INFO          0xA1
#define DECK_MSG_GET_STRING_ATTRIBUTE     0xAE
#define DECK_MSG_TRIGGER_HAPTIC_CMD       0xEA
#define DECK_MSG_TRIGGER_RUMBLE_CMD       0xEB

// String-attribute selectors carried in the args of GET_STRING_ATTRIBUTE.
#define DECK_ATTRIB_STR_BOARD_SERIAL      0x00
#define DECK_ATTRIB_STR_UNIT_SERIAL       0x01

// Numeric attribute selectors returned by GET_ATTRIBUTES_VALUES.
#define DECK_ATTRIB_UNIQUE_ID             0x00
#define DECK_ATTRIB_PRODUCT_ID            0x01
#define DECK_ATTRIB_CAPABILITIES          0x02
#define DECK_ATTRIB_FIRMWARE_BUILD_TIME   0x04
#define DECK_ATTRIB_BOARD_REVISION        0x09
#define DECK_ATTRIB_BOOTLOADER_BUILD_TIME 0x0A

// ---------------------------------------------------------------------------
// Build a canned 64-byte feature-report reply for `msgId` into `out`.
//
// The Steam Controller protocol is set-then-get over feature reports: host
// SETs a request [msg_id, len, ...args], then GETs the reply [msg_id, len,
// ...response]. Steam parses the reply by checking out[0] == msg_id, then
// walking the payload according to the message's known schema.
//
// For step 4 (recognition) we satisfy the messages Steam fires during open:
//
//   - CLEAR_DIGITAL_MAPPINGS  : ack-only (echo with len=0)
//   - LOAD_DEFAULT_SETTINGS    : ack-only
//   - SET_SETTINGS_VALUES      : ack-only
//   - GET_ATTRIBUTES_VALUES    : return one fake attribute (PRODUCT_ID = 0x1205)
//                                so Steam's parser advances past the read
//                                without erroring out
//   - GET_STRING_ATTRIBUTE     : if asked for UNIT_SERIAL, return our serial
//                                string ("MEDA00000001")
//   - everything else          : ack-only
// ---------------------------------------------------------------------------
static VOID
BuildFeatureResponse(_In_reads_bytes_(DECK_OUTPUT_REPORT_SIZE) const UCHAR *request,
                     _In_ ULONG  requestLen,
                     _Out_writes_bytes_all_(DECK_OUTPUT_REPORT_SIZE) UCHAR *out)
{
    RtlZeroMemory(out, DECK_OUTPUT_REPORT_SIZE);

    if (requestLen < 2) {
        return;
    }

    const UCHAR  msgId = request[0];
    const UCHAR  argLen = request[1];
    const UCHAR *args = request + 2;
    const ULONG  argsAvail = (requestLen >= 2) ? (requestLen - 2) : 0;

    out[0] = msgId;
    out[1] = 0;  // overwritten below for messages that return a payload

    switch (msgId) {
    case DECK_MSG_GET_ATTRIBUTES_VALUES: {
        // Reply schema: [0x83, len, (attr_id:1, value:4) * N]
        // Returning a single PRODUCT_ID attribute is enough for Steam's
        // GetControllerInfo work item to advance — it parses opportunistically.
        out[1] = 5;
        out[2] = DECK_ATTRIB_PRODUCT_ID;
        out[3] = 0x05;          // 0x00001205 little-endian
        out[4] = 0x12;
        out[5] = 0x00;
        out[6] = 0x00;
        break;
    }

    case DECK_MSG_GET_STRING_ATTRIBUTE: {
        // Reply schema: [0xAE, len, attr_id, ...ASCII...]
        if (argLen >= 1 && argsAvail >= 1 && args[0] == DECK_ATTRIB_STR_UNIT_SERIAL) {
            static const CHAR serial[] = "MEDA00000001";
            const UCHAR slen = static_cast<UCHAR>(sizeof(serial) - 1);  // sans NUL
            out[1] = static_cast<UCHAR>(slen + 1);  // attr_id + string bytes
            out[2] = DECK_ATTRIB_STR_UNIT_SERIAL;
            RtlCopyMemory(out + 3, serial, slen);
        } else {
            // Unknown string attribute — empty payload.
            out[1] = 0;
        }
        break;
    }

    case DECK_MSG_GET_DEVICE_INFO:
    case DECK_MSG_CLEAR_DIGITAL_MAPPINGS:
    case DECK_MSG_SET_DEFAULT_DIGITAL_MAPPINGS:
    case DECK_MSG_SET_SETTINGS_VALUES:
    case DECK_MSG_LOAD_DEFAULT_SETTINGS:
    case DECK_MSG_TRIGGER_HAPTIC_PULSE:
    case DECK_MSG_TRIGGER_HAPTIC_CMD:
    case DECK_MSG_TRIGGER_RUMBLE_CMD:
    default:
        // Ack-only: echo msg_id with zero-length payload.
        // TODO: route haptic/rumble payloads to PendedOutputQueue so
        // user-mode can ship them back to the Deck.
        break;
    }
}

EVT_WDF_IO_QUEUE_IO_INTERNAL_DEVICE_CONTROL EvtControlUrb;

VOID
EvtControlUrb(_In_ WDFQUEUE Queue,
              _In_ WDFREQUEST Request,
              _In_ size_t /*OutputBufferLength*/,
              _In_ size_t /*InputBufferLength*/,
              _In_ ULONG IoControlCode)
{
    UNREFERENCED_PARAMETER(Queue);
    NT_VERIFY(IoControlCode == IOCTL_INTERNAL_USB_SUBMIT_URB);

    WDF_USB_CONTROL_SETUP_PACKET setup;
    NTSTATUS                     status;
    PUCHAR                       buffer = nullptr;
    ULONG                        bufferLength = 0;
    ULONG                        bytesCompleted = 0;

    // The buffer may be absent (e.g. SET_IDLE has none); ignore failure.
    if (!NT_SUCCESS(UdecxUrbRetrieveBuffer(Request, &buffer, &bufferLength))) {
        buffer = nullptr;
        bufferLength = 0;
    }

    status = UdecxUrbRetrieveControlSetupPacket(Request, &setup);
    if (!NT_SUCCESS(status)) {
        UdecxUrbCompleteWithNtStatus(Request, status);
        return;
    }

    const UCHAR  type      = setup.Packet.bm.Request.Type;
    const UCHAR  recipient = setup.Packet.bm.Request.Recipient;
    const UCHAR  bRequest  = setup.Packet.bRequest;
    const USHORT wValue    = setup.Packet.wValue.Value;
    const USHORT wIndex    = setup.Packet.wIndex.Value;
    const USHORT wLength   = setup.Packet.wLength;

    status = STATUS_INVALID_DEVICE_REQUEST;

    // ------- Standard GET_DESCRIPTOR to interface (HID class descriptors) -------
    if (type == BMREQUEST_STANDARD &&
        recipient == BMREQUEST_TO_INTERFACE &&
        bRequest == USB_REQUEST_GET_DESCRIPTOR)
    {
        const UCHAR descType  = static_cast<UCHAR>(wValue >> 8);
        const PUCHAR src      = nullptr;
        ULONG  srcLen         = 0;
        const UCHAR *srcPtr   = nullptr;

        if (descType == HID_DESCRIPTOR_TYPE_REPORT) {
            switch (wIndex) {
            case 0:
                srcPtr = g_If0HidReportDescriptor;
                srcLen = sizeof(g_If0HidReportDescriptor);
                break;
            case 1:
                srcPtr = g_If1HidReportDescriptor;
                srcLen = sizeof(g_If1HidReportDescriptor);
                break;
            case 2:
                srcPtr = g_If2HidReportDescriptor;
                srcLen = sizeof(g_If2HidReportDescriptor);
                break;
            }
        }

        if (srcPtr != nullptr) {
            const ULONG copyLen = (wLength < srcLen) ? wLength : srcLen;
            if (buffer != nullptr && bufferLength >= copyLen) {
                RtlCopyMemory(buffer, srcPtr, copyLen);
                bytesCompleted = copyLen;
                status = STATUS_SUCCESS;
            } else {
                status = STATUS_BUFFER_TOO_SMALL;
            }
        }
        // descType == HID descriptor (0x21) falls through — already in our
        // config descriptor, host should not normally request it standalone.
        UNREFERENCED_PARAMETER(src);
    }

    // ------- HID class requests to interface -------
    else if (type == BMREQUEST_CLASS && recipient == BMREQUEST_TO_INTERFACE) {
        PDEVICE_CONTEXT ctx = DeviceGetContext(WdfIoQueueGetDevice(Queue));
        const UCHAR reportType = static_cast<UCHAR>(wValue >> 8);

        switch (bRequest) {
        case HID_REQUEST_SET_IDLE:
        case HID_REQUEST_SET_PROTOCOL:
            // Plain ack — Microsoft HID class driver fires these during init.
            status = STATUS_SUCCESS;
            break;

        case HID_REQUEST_SET_REPORT:
            // Feature reports on IF2 are the Steam Controller request channel
            // (set-then-get pattern). Build the corresponding canned reply
            // and stash it for the next GET_REPORT.
            //
            // TODO: when the request is a haptic / rumble command
            // (DECK_MSG_TRIGGER_HAPTIC_CMD/PULSE/RUMBLE), forward the args
            // to PendedOutputQueue so user-mode can ship rumble back to
            // the Deck.
            if (reportType == HID_REPORT_TYPE_FEATURE && buffer != nullptr) {
                BuildFeatureResponse(buffer, bufferLength, ctx->LastFeatureResponse);
            }
            status = STATUS_SUCCESS;
            break;

        case HID_REQUEST_GET_IDLE:
        case HID_REQUEST_GET_PROTOCOL:
            // Return one byte of zero. Both fields default to 0.
            if (buffer != nullptr && bufferLength >= 1) {
                buffer[0] = 0;
                bytesCompleted = 1;
                status = STATUS_SUCCESS;
            } else {
                status = STATUS_BUFFER_TOO_SMALL;
            }
            break;

        case HID_REQUEST_GET_REPORT:
            // Feature reads return the canned response built during the
            // preceding SET_REPORT. Input/Output reads return zeros — input
            // flows over the interrupt-IN endpoint instead.
            if (buffer != nullptr) {
                if (reportType == HID_REPORT_TYPE_FEATURE) {
                    const ULONG copyLen =
                        (wLength < DECK_OUTPUT_REPORT_SIZE) ? wLength : DECK_OUTPUT_REPORT_SIZE;
                    if (bufferLength >= copyLen) {
                        RtlCopyMemory(buffer, ctx->LastFeatureResponse, copyLen);
                        bytesCompleted = copyLen;
                        status = STATUS_SUCCESS;
                    } else {
                        status = STATUS_BUFFER_TOO_SMALL;
                    }
                } else {
                    if (bufferLength >= wLength) {
                        RtlZeroMemory(buffer, wLength);
                        bytesCompleted = wLength;
                        status = STATUS_SUCCESS;
                    } else {
                        status = STATUS_BUFFER_TOO_SMALL;
                    }
                }
            } else {
                status = STATUS_BUFFER_TOO_SMALL;
            }
            break;

        default:
            // Unknown class request — fail it.
            break;
        }
    }

    if (NT_SUCCESS(status)) {
        UdecxUrbSetBytesCompleted(Request, bytesCompleted);
    }
    UdecxUrbCompleteWithNtStatus(Request, status);
}

// ===========================================================================
// Endpoint creation helpers.
// ===========================================================================

// Allocate + create one simple endpoint (interrupt or bulk). Sets up a
// manual-dispatch WDFQUEUE for it and stores both handles in the caller's
// out-pointers. Caller frees `endpointInit` on failure.
static NTSTATUS
CreateSimpleEndpoint(_In_  PDEVICE_CONTEXT  ctx,
                     _In_  WDFDEVICE        wdfDevice,
                     _In_  UCHAR            address,
                     _Out_ UDECXUSBENDPOINT *endpointOut,
                     _Out_opt_ WDFQUEUE     *queueOut)
{
    NTSTATUS                  status;
    PUDECXUSBENDPOINT_INIT    epInit = nullptr;
    UDECX_USB_ENDPOINT_CALLBACKS epCallbacks;
    WDF_IO_QUEUE_CONFIG       queueConfig;
    WDFQUEUE                  queue = nullptr;

    *endpointOut = nullptr;
    if (queueOut != nullptr) *queueOut = nullptr;

    // Per-endpoint queue. Manual dispatch — interrupt-IN URBs sit here
    // until either user-mode pushes input bytes (gamepad path) or the
    // host gives up. The other endpoints' queues stay full of pending
    // URBs that simply never complete (host eventually treats them as
    // idle); this is fine for our use case.
    WDF_IO_QUEUE_CONFIG_INIT(&queueConfig, WdfIoQueueDispatchManual);
    status = WdfIoQueueCreate(wdfDevice, &queueConfig, WDF_NO_OBJECT_ATTRIBUTES, &queue);
    if (!NT_SUCCESS(status)) goto fail;

    epInit = UdecxUsbSimpleEndpointInitAllocate(ctx->VirtualUsbDevice);
    if (epInit == nullptr) { status = STATUS_INSUFFICIENT_RESOURCES; goto fail; }

    UdecxUsbEndpointInitSetEndpointAddress(epInit, address);
    UDECX_USB_ENDPOINT_CALLBACKS_INIT(&epCallbacks, EvtUsbEndpointReset);
    UdecxUsbEndpointInitSetCallbacks(epInit, &epCallbacks);

    status = UdecxUsbEndpointCreate(&epInit, WDF_NO_OBJECT_ATTRIBUTES, endpointOut);
    if (!NT_SUCCESS(status)) goto fail;

    UdecxUsbEndpointSetWdfIoQueue(*endpointOut, queue);

    if (queueOut != nullptr) *queueOut = queue;
    return STATUS_SUCCESS;

fail:
    if (epInit != nullptr) UdecxUsbEndpointInitFree(epInit);
    return status;
}

// Create the default control endpoint (EP0). Sequential dispatch through
// EvtControlUrb, which services HID class + standard-to-interface requests.
static NTSTATUS
CreateControlEndpoint(_In_ PDEVICE_CONTEXT ctx, _In_ WDFDEVICE wdfDevice)
{
    NTSTATUS                  status;
    PUDECXUSBENDPOINT_INIT    epInit = nullptr;
    UDECX_USB_ENDPOINT_CALLBACKS epCallbacks;
    WDF_IO_QUEUE_CONFIG       queueConfig;

    WDF_IO_QUEUE_CONFIG_INIT(&queueConfig, WdfIoQueueDispatchSequential);
    queueConfig.EvtIoInternalDeviceControl = EvtControlUrb;

    status = WdfIoQueueCreate(wdfDevice, &queueConfig,
                              WDF_NO_OBJECT_ATTRIBUTES, &ctx->ControlEndpointQueue);
    if (!NT_SUCCESS(status)) return status;

    epInit = UdecxUsbSimpleEndpointInitAllocate(ctx->VirtualUsbDevice);
    if (epInit == nullptr) return STATUS_INSUFFICIENT_RESOURCES;

    UdecxUsbEndpointInitSetEndpointAddress(epInit, USB_DEFAULT_ENDPOINT_ADDRESS);
    UDECX_USB_ENDPOINT_CALLBACKS_INIT(&epCallbacks, EvtUsbEndpointReset);
    UdecxUsbEndpointInitSetCallbacks(epInit, &epCallbacks);

    status = UdecxUsbEndpointCreate(&epInit, WDF_NO_OBJECT_ATTRIBUTES, &ctx->ControlEp);
    if (!NT_SUCCESS(status)) {
        UdecxUsbEndpointInitFree(epInit);
        return status;
    }

    UdecxUsbEndpointSetWdfIoQueue(ctx->ControlEp, ctx->ControlEndpointQueue);
    return STATUS_SUCCESS;
}

// ===========================================================================
// UsbDeviceCreate — orchestrates the full UDE bring-up.
// ===========================================================================

NTSTATUS UsbDeviceCreate(_In_ WDFDEVICE Device)
{
    PDEVICE_CONTEXT ctx = DeviceGetContext(Device);
    NTSTATUS        status;

    ctx->UdecxUsbDeviceInit = UdecxUsbDeviceInitAllocate(Device);
    if (ctx->UdecxUsbDeviceInit == nullptr) {
        return STATUS_INSUFFICIENT_RESOURCES;
    }

    UDECX_USB_DEVICE_STATE_CHANGE_CALLBACKS stateCallbacks;
    UDECX_USB_DEVICE_CALLBACKS_INIT(&stateCallbacks);
    stateCallbacks.EvtUsbDeviceLinkPowerEntry            = EvtUsbDeviceLinkPowerEntry;
    stateCallbacks.EvtUsbDeviceLinkPowerExit             = EvtUsbDeviceLinkPowerExit;
    stateCallbacks.EvtUsbDeviceSetFunctionSuspendAndWake = EvtUsbDeviceSetFunctionSuspendAndWake;
    UdecxUsbDeviceInitSetStateChangeCallbacks(ctx->UdecxUsbDeviceInit, &stateCallbacks);

    UdecxUsbDeviceInitSetSpeed(ctx->UdecxUsbDeviceInit, UdecxUsbHighSpeed);
    UdecxUsbDeviceInitSetEndpointsType(ctx->UdecxUsbDeviceInit, UdecxEndpointTypeSimple);

    // Standard descriptors — UDE services GET_DESCRIPTOR(DEVICE/CONFIG)
    // automatically once registered.
    status = UdecxUsbDeviceInitAddDescriptor(ctx->UdecxUsbDeviceInit,
                                             const_cast<PUCHAR>(g_DeviceDescriptor),
                                             sizeof(g_DeviceDescriptor));
    if (!NT_SUCCESS(status)) goto fail;

    status = UdecxUsbDeviceInitAddDescriptor(ctx->UdecxUsbDeviceInit,
                                             const_cast<PUCHAR>(g_ConfigDescriptor),
                                             sizeof(g_ConfigDescriptor));
    if (!NT_SUCCESS(status)) goto fail;

    // Lang-ID descriptor (string index 0).
    status = UdecxUsbDeviceInitAddDescriptorWithIndex(ctx->UdecxUsbDeviceInit,
                                                     const_cast<PUCHAR>(g_LanguageDescriptor),
                                                     sizeof(g_LanguageDescriptor),
                                                     0);
    if (!NT_SUCCESS(status)) goto fail;

    // Manufacturer / Product / Serial.
    status = UdecxUsbDeviceInitAddStringDescriptor(ctx->UdecxUsbDeviceInit,
                                                   &g_ManufacturerString,
                                                   DECK_STR_MANUFACTURER,
                                                   DECK_LANG_ID_EN_US);
    if (!NT_SUCCESS(status)) goto fail;

    status = UdecxUsbDeviceInitAddStringDescriptor(ctx->UdecxUsbDeviceInit,
                                                   &g_ProductString,
                                                   DECK_STR_PRODUCT,
                                                   DECK_LANG_ID_EN_US);
    if (!NT_SUCCESS(status)) goto fail;

    status = UdecxUsbDeviceInitAddStringDescriptor(ctx->UdecxUsbDeviceInit,
                                                   &g_SerialString,
                                                   DECK_STR_SERIAL,
                                                   DECK_LANG_ID_EN_US);
    if (!NT_SUCCESS(status)) goto fail;

    // Materialize the virtual device. UdecxUsbDeviceCreate consumes
    // UdecxUsbDeviceInit on success.
    {
        WDF_OBJECT_ATTRIBUTES devAttrs;
        WDF_OBJECT_ATTRIBUTES_INIT(&devAttrs);
        status = UdecxUsbDeviceCreate(&ctx->UdecxUsbDeviceInit, &devAttrs, &ctx->VirtualUsbDevice);
        if (!NT_SUCCESS(status)) goto fail;
    }

    // Endpoints. Order: control first, then the data endpoints.
    status = CreateControlEndpoint(ctx, Device);
    if (!NT_SUCCESS(status)) return status;

    // Three HID interrupt-IN endpoints. Only the gamepad path's queue is
    // load-bearing for our IOCTL routing; we still hold queue handles for
    // the others so they don't error on URB submission.
    status = CreateSimpleEndpoint(ctx, Device, 0x82, &ctx->KbdInEp,     nullptr);
    if (!NT_SUCCESS(status)) return status;
    status = CreateSimpleEndpoint(ctx, Device, 0x81, &ctx->MouseInEp,   nullptr);
    if (!NT_SUCCESS(status)) return status;
    status = CreateSimpleEndpoint(ctx, Device, 0x83, &ctx->GamepadInEp, &ctx->GamepadInQueue);
    if (!NT_SUCCESS(status)) return status;

    // CDC ACM endpoints. Inert — no one opens the COM port.
    status = CreateSimpleEndpoint(ctx, Device, 0x84, &ctx->CdcNotifyEp,  nullptr);
    if (!NT_SUCCESS(status)) return status;
    status = CreateSimpleEndpoint(ctx, Device, 0x85, &ctx->CdcDataInEp,  nullptr);
    if (!NT_SUCCESS(status)) return status;
    status = CreateSimpleEndpoint(ctx, Device, 0x05, &ctx->CdcDataOutEp, nullptr);
    if (!NT_SUCCESS(status)) return status;

    // Plug in. Windows enumerates the device after this returns.
    {
        UDECX_USB_DEVICE_PLUG_IN_OPTIONS plugInOptions;
        UDECX_USB_DEVICE_PLUG_IN_OPTIONS_INIT(&plugInOptions);
        plugInOptions.Usb20PortNumber = 1;
        return UdecxUsbDevicePlugIn(ctx->VirtualUsbDevice, &plugInOptions);
    }

fail:
    if (ctx->UdecxUsbDeviceInit != nullptr) {
        UdecxUsbDeviceInitFree(ctx->UdecxUsbDeviceInit);
        ctx->UdecxUsbDeviceInit = nullptr;
    }
    return status;
}
