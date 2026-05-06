// User-mode IOCTL surface.
//
// IOCTLs are defined in inc/public.h and mirrored in client-win.
//
// Design:
//   - IOCTL_DECK_PUSH_INPUT_REPORT: synchronous, completes immediately
//     after handing bytes to the virtual interrupt-IN endpoint.
//   - IOCTL_DECK_PEND_OUTPUT_REPORT: parked on PendedOutputQueue (manual
//     dispatch). Completes when the host writes a feature/output report
//     (rumble/haptics) to the virtual device — i.e. when the
//     interrupt-OUT-ready or feature-set callback fires inside usbdevice.cpp.

#include "common.h"

NTSTATUS QueueInitialize(_In_ WDFDEVICE Device)
{
    PDEVICE_CONTEXT ctx = DeviceGetContext(Device);

    // Default queue: dispatches DeviceIoControl from any handle.
    WDF_IO_QUEUE_CONFIG queueCfg;
    WDF_IO_QUEUE_CONFIG_INIT_DEFAULT_QUEUE(&queueCfg, WdfIoQueueDispatchParallel);
    queueCfg.EvtIoDeviceControl = EvtIoDeviceControl;

    NTSTATUS status = WdfIoQueueCreate(Device,
                                       &queueCfg,
                                       WDF_NO_OBJECT_ATTRIBUTES,
                                       &ctx->IoctlQueue);
    if (!NT_SUCCESS(status)) return status;

    // Manual queue for parked PEND_OUTPUT requests. We complete these from
    // usbdevice.cpp when the host pushes a feature report at us.
    WDF_IO_QUEUE_CONFIG_INIT(&queueCfg, WdfIoQueueDispatchManual);
    return WdfIoQueueCreate(Device,
                            &queueCfg,
                            WDF_NO_OBJECT_ATTRIBUTES,
                            &ctx->PendedOutputQueue);
}

VOID
EvtIoDeviceControl(_In_ WDFQUEUE Queue,
                   _In_ WDFREQUEST Request,
                   _In_ size_t OutputBufferLength,
                   _In_ size_t InputBufferLength,
                   _In_ ULONG IoControlCode)
{
    PDEVICE_CONTEXT ctx = DeviceGetContext(WdfIoQueueGetDevice(Queue));
    NTSTATUS status;

    switch (IoControlCode)
    {
    case IOCTL_DECK_PUSH_INPUT_REPORT:
    {
        if (InputBufferLength != DECK_INPUT_REPORT_SIZE) {
            status = STATUS_INVALID_BUFFER_SIZE;
            break;
        }
        PVOID buf;
        status = WdfRequestRetrieveInputBuffer(Request,
                                               DECK_INPUT_REPORT_SIZE,
                                               &buf,
                                               nullptr);
        if (!NT_SUCCESS(status)) break;

        // TODO: hand `buf` (DECK_INPUT_REPORT_SIZE bytes) to the
        // interrupt-IN endpoint of ctx->VirtualUsbDevice. Likely shape:
        //   - look up the WDFREQUEST queued on the endpoint by the host;
        //   - copy our bytes into its output buffer;
        //   - WdfRequestComplete that endpoint-side request;
        //   - status = STATUS_SUCCESS.
        UNREFERENCED_PARAMETER(ctx);
        status = STATUS_NOT_IMPLEMENTED;
        break;
    }

    case IOCTL_DECK_PEND_OUTPUT_REPORT:
    {
        if (OutputBufferLength == 0) {
            status = STATUS_INVALID_BUFFER_SIZE;
            break;
        }
        // Park the request on the manual queue. usbdevice.cpp completes it
        // when an output report arrives.
        status = WdfRequestForwardToIoQueue(Request, ctx->PendedOutputQueue);
        if (NT_SUCCESS(status)) return; // request now owns its lifetime
        break;
    }

    default:
        status = STATUS_INVALID_DEVICE_REQUEST;
        break;
    }

    WdfRequestComplete(Request, status);
}
