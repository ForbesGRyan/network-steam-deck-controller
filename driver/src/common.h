// Shared kernel-side declarations.

#pragma once

#include <ntddk.h>
#include <wdf.h>
#include <usb.h>
#include <usbdi.h>
#include <wdfusb.h>
#include <Udecx.h>

#include "../inc/public.h"

// Per-device context. Hangs off the WDFDEVICE the framework gives us.
typedef struct _DEVICE_CONTEXT
{
    // UDE init handle — kept around so we can free it on the failure path
    // (it is consumed by UdecxUsbDeviceCreate on success).
    PUDECXUSBDEVICE_INIT UdecxUsbDeviceInit;

    // The "Deck" we present to Windows.
    UDECXUSBDEVICE       VirtualUsbDevice;

    // Default queue: dispatches user-mode IOCTLs from client-win.
    WDFQUEUE             IoctlQueue;

    // Manual queue parking PEND_OUTPUT IOCTLs awaiting a feature/output
    // report from the host.
    WDFQUEUE             PendedOutputQueue;

    // Control endpoint (EP0) queue. Sequential dispatch; entries arrive as
    // IOCTL_INTERNAL_USB_SUBMIT_URB. We service HID class + standard-to-
    // interface requests here (notably GET_DESCRIPTOR(REPORT) and SET_IDLE).
    WDFQUEUE             ControlEndpointQueue;

    // UDE endpoint handles, one per address declared in the config descriptor.
    UDECXUSBENDPOINT     ControlEp;          // EP 0x00 control
    UDECXUSBENDPOINT     KbdInEp;            // EP 0x82  IF0 keyboard interrupt-IN
    UDECXUSBENDPOINT     MouseInEp;          // EP 0x81  IF1 mouse interrupt-IN
    UDECXUSBENDPOINT     GamepadInEp;        // EP 0x83  IF2 gamepad interrupt-IN  (load-bearing)
    UDECXUSBENDPOINT     CdcNotifyEp;        // EP 0x84  IF3 CDC notification interrupt-IN
    UDECXUSBENDPOINT     CdcDataInEp;        // EP 0x85  IF4 CDC bulk-IN
    UDECXUSBENDPOINT     CdcDataOutEp;       // EP 0x05  IF4 CDC bulk-OUT

    // Endpoint queues for the gamepad path. Manual dispatch. The WDFREQUEST
    // sitting at the head is the host's pending interrupt-IN URB, waiting
    // for input report bytes; IOCTL_DECK_PUSH_INPUT_REPORT pulls it and
    // completes it with the user-mode-supplied buffer.
    WDFQUEUE             GamepadInQueue;
} DEVICE_CONTEXT, *PDEVICE_CONTEXT;

WDF_DECLARE_CONTEXT_TYPE_WITH_NAME(DEVICE_CONTEXT, DeviceGetContext)

// driver.cpp
EVT_WDF_DRIVER_DEVICE_ADD                EvtDriverDeviceAdd;
EVT_UDECX_WDF_DEVICE_QUERY_USB_CAPABILITY EvtControllerQueryUsbCapability;

// queue.cpp
NTSTATUS QueueInitialize(_In_ WDFDEVICE Device);
EVT_WDF_IO_QUEUE_IO_DEVICE_CONTROL EvtIoDeviceControl;

// usbdevice.cpp
NTSTATUS UsbDeviceCreate(_In_ WDFDEVICE Device);

// Convenience for placeholder bodies during scaffolding.
#define TODO_NOT_IMPLEMENTED() \
    do { DbgPrint("[network-deck] TODO: %s\n", __FUNCTION__); \
         return STATUS_NOT_IMPLEMENTED; } while (0)
