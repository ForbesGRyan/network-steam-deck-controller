// Shared kernel-side declarations.

#pragma once

#include <ntddk.h>
#include <wdf.h>
#include <Udecx.h>

#include "../inc/public.h"

// Per-device context. Hangs off the WDFDEVICE the framework gives us.
typedef struct _DEVICE_CONTEXT
{
    UDECXUSBDEVICE  VirtualUsbDevice;   // The "Deck" we present to Windows.
    WDFQUEUE        IoctlQueue;         // Default queue for user-mode IOCTLs.
    WDFQUEUE        PendedOutputQueue;  // Manual queue parked PEND_OUTPUT IOCTLs.
} DEVICE_CONTEXT, *PDEVICE_CONTEXT;

WDF_DECLARE_CONTEXT_TYPE_WITH_NAME(DEVICE_CONTEXT, DeviceGetContext)

// driver.cpp
EVT_WDF_DRIVER_DEVICE_ADD EvtDriverDeviceAdd;

// queue.cpp
NTSTATUS QueueInitialize(_In_ WDFDEVICE Device);
EVT_WDF_IO_QUEUE_IO_DEVICE_CONTROL EvtIoDeviceControl;

// usbdevice.cpp
NTSTATUS UsbDeviceCreate(_In_ WDFDEVICE Device);

// Convenience for placeholder bodies during scaffolding.
#define TODO_NOT_IMPLEMENTED() \
    do { DbgPrint("[network-deck] TODO: %s\n", __FUNCTION__); \
         return STATUS_NOT_IMPLEMENTED; } while (0)
