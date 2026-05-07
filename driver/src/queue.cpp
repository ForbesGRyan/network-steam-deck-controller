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

        // Pull the next host URB from the gamepad EP 0x83 IN queue. The
        // host's USB stack submits these every ~1 ms (our descriptor's
        // bInterval); they sit on this manual queue waiting for input.
        WDFREQUEST epRequest;
        NTSTATUS dq = WdfIoQueueRetrieveNextRequest(ctx->GamepadInQueue,
                                                    &epRequest);
        if (dq == STATUS_NO_MORE_ENTRIES) {
            // No host URB pending — drop this report. At ~250 Hz from
            // user-mode and ~1 kHz host polling we should normally have
            // a URB ready; a drop here just means the host already has
            // a fresher one in flight from some prior IOCTL.
            status = STATUS_SUCCESS;
            break;
        }
        if (!NT_SUCCESS(dq)) {
            status = dq;
            break;
        }

        // Copy our 64 bytes into the URB's transfer buffer and complete it.
        PUCHAR urbBuf;
        ULONG  urbLen;
        NTSTATUS retrieve = UdecxUrbRetrieveBuffer(epRequest, &urbBuf, &urbLen);
        if (!NT_SUCCESS(retrieve)) {
            UdecxUrbCompleteWithNtStatus(epRequest, retrieve);
            status = retrieve;
            break;
        }

        const ULONG copyLen = (urbLen < DECK_INPUT_REPORT_SIZE)
                                  ? urbLen : DECK_INPUT_REPORT_SIZE;
        RtlCopyMemory(urbBuf, buf, copyLen);
        UdecxUrbSetBytesCompleted(epRequest, copyLen);
        UdecxUrbCompleteWithNtStatus(epRequest, STATUS_SUCCESS);

        status = STATUS_SUCCESS;
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
