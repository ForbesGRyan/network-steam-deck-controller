// DriverEntry + per-device add. Keep the surface area here small and
// delegate UDE setup to usbdevice.cpp, IOCTLs to queue.cpp.
//
// API reference:
//   - microsoft/UDE  UDEMbimClientSample/{driver,controller,usbdevice}.cpp
//   - https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/udecx/

// INITGUID must precede the first include of any header that uses
// DEFINE_GUID (for us: public.h). Defining it here causes storage for
// GUID_DEVINTERFACE_DECK_VIRTUAL to be emitted in this translation unit.
// Other .cpp files transitively pulling public.h get extern declarations,
// linker resolves to this definition.
#include <initguid.h>
#include "common.h"

extern "C" DRIVER_INITIALIZE DriverEntry;

extern "C" NTSTATUS
DriverEntry(_In_ PDRIVER_OBJECT DriverObject, _In_ PUNICODE_STRING RegistryPath)
{
    WDF_DRIVER_CONFIG config;
    WDF_DRIVER_CONFIG_INIT(&config, EvtDriverDeviceAdd);

    return WdfDriverCreate(DriverObject,
                           RegistryPath,
                           WDF_NO_OBJECT_ATTRIBUTES,
                           &config,
                           WDF_NO_HANDLE);
}

NTSTATUS
EvtDriverDeviceAdd(_In_ WDFDRIVER Driver, _Inout_ PWDFDEVICE_INIT DeviceInit)
{
    UNREFERENCED_PARAMETER(Driver);

    NTSTATUS status;

    // Tell the framework we'll be a UDE controller. Must precede WdfDeviceCreate.
    status = UdecxInitializeWdfDeviceInit(DeviceInit);
    if (!NT_SUCCESS(status)) return status;

    WDF_OBJECT_ATTRIBUTES deviceAttrs;
    WDF_OBJECT_ATTRIBUTES_INIT_CONTEXT_TYPE(&deviceAttrs, DEVICE_CONTEXT);

    WDFDEVICE device;
    status = WdfDeviceCreate(&DeviceInit, &deviceAttrs, &device);
    if (!NT_SUCCESS(status)) return status;

    // Register as a UDE-capable WDF device. The query-capability callback
    // is required by the framework — UCX consults it to learn what speeds
    // and features we expose. We claim "no extra capabilities" for now.
    UDECX_WDF_DEVICE_CONFIG udeConfig;
    UDECX_WDF_DEVICE_CONFIG_INIT(&udeConfig, EvtControllerQueryUsbCapability);

    status = UdecxWdfDeviceAddUsbDeviceEmulation(device, &udeConfig);
    if (!NT_SUCCESS(status)) return status;

    // Expose a device interface so user-mode (client-win) can find us via
    // SetupDiEnumDeviceInterfaces.
    status = WdfDeviceCreateDeviceInterface(device,
                                            &GUID_DEVINTERFACE_DECK_VIRTUAL,
                                            nullptr);
    if (!NT_SUCCESS(status)) return status;

    status = QueueInitialize(device);
    if (!NT_SUCCESS(status)) return status;

    // Build the virtual Deck and plug it into our virtual host controller.
    // Windows now sees a USB device with VID 0x28de PID 0x1205 appear.
    return UsbDeviceCreate(device);
}

NTSTATUS
EvtControllerQueryUsbCapability(_In_ WDFDEVICE UdecxWdfDevice,
                                _In_ PGUID     CapabilityType,
                                _In_ ULONG     OutputBufferLength,
                                _Out_writes_to_opt_(OutputBufferLength, *ResultLength) PVOID OutputBuffer,
                                _Out_ PULONG   ResultLength)
{
    UNREFERENCED_PARAMETER(UdecxWdfDevice);
    UNREFERENCED_PARAMETER(CapabilityType);
    UNREFERENCED_PARAMETER(OutputBufferLength);
    UNREFERENCED_PARAMETER(OutputBuffer);

    // We don't expose any optional UCX capabilities (no static streams,
    // no chained MDLs, no function suspend, etc.). Return zero length and
    // STATUS_NOT_IMPLEMENTED for every query — UCX falls back to defaults.
    *ResultLength = 0;
    return STATUS_NOT_IMPLEMENTED;
}
