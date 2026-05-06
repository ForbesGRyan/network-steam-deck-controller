// Virtual USB device (the "Deck") definition + plug-in.
//
// What this file owns when fully implemented:
//   - USB device, configuration, interface, HID, and endpoint descriptors
//     mirroring the real Steam Deck controller (VID 0x28de PID 0x1205).
//   - UdecxUsbDeviceCreate + descriptor registration calls.
//   - Endpoint queues — interrupt-IN for input reports, optional
//     interrupt-OUT for output reports.
//   - UdecxUsbDevicePlugIn to make the device visible to Windows.
//   - Endpoint-ready callback that pulls reports off PendedOutputQueue
//     (haptics path) and feeds reports queued by user-mode (input path).
//
// Reference: Microsoft Windows-driver-samples/usb/UDE_* — copy the
// descriptor-array pattern and adapt to our descriptors.

#include "common.h"

// ---------------------------------------------------------------------------
// USB descriptors. PLACEHOLDERS. Capture from a real Deck on Windows
// (USBView, or the WDK's UVCView, or `Get-PnpDeviceProperty`) and replace.
// Steam Input fingerprints VID/PID + descriptor set; getting these wrong
// silently breaks recognition.
// ---------------------------------------------------------------------------

#define DECK_VID 0x28de
#define DECK_PID 0x1205

// TODO: real descriptors. The skeletons below are illustrative byte counts
// only — they will not pass USB enumeration as-is.

static const UCHAR g_DeviceDescriptor[] = {
    0x12,           // bLength
    0x01,           // bDescriptorType = DEVICE
    0x00, 0x02,     // bcdUSB 2.00
    0x00, 0x00, 0x00, // class/subclass/protocol — TODO from capture
    0x40,           // bMaxPacketSize0 = 64
    (DECK_VID & 0xFF), ((DECK_VID >> 8) & 0xFF),
    (DECK_PID & 0xFF), ((DECK_PID >> 8) & 0xFF),
    0x00, 0x01,     // bcdDevice — TODO match real Deck
    0x00, 0x00, 0x00, // iManufacturer/iProduct/iSerialNumber — TODO
    0x01,           // bNumConfigurations
};

// TODO: configuration descriptor (interface(s), HID descriptor, endpoints).
// TODO: HID report descriptor.

// ---------------------------------------------------------------------------

NTSTATUS UsbDeviceCreate(_In_ WDFDEVICE Device)
{
    UNREFERENCED_PARAMETER(Device);
    UNREFERENCED_PARAMETER(g_DeviceDescriptor);

    // TODO:
    //   1. UdecxUsbDeviceInitAllocate
    //   2. UdecxUsbDeviceInitSetSpeed (USB_SUPER_SPEED or USB_HIGH_SPEED)
    //   3. Register descriptors:
    //      UdecxUsbDeviceInitAddDescriptor (device, configuration, string)
    //   4. Register endpoint callbacks (EvtUsbEndpointReady, etc.)
    //   5. UdecxUsbDeviceCreate
    //   6. Create endpoints (UdecxUsbEndpointCreate) — control + interrupt-IN
    //      (and interrupt-OUT if the real Deck exposes one for output reports).
    //   7. UdecxUsbDevicePlugIn — Windows now sees the Deck.
    //
    // Save the resulting UDECXUSBDEVICE in DeviceGetContext(Device)->VirtualUsbDevice
    // so queue.cpp can route IOCTLs to it.

    TODO_NOT_IMPLEMENTED();
}
