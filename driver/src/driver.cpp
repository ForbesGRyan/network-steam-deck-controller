// DriverEntry + per-device add. Keep the surface area here small and
// delegate UDE setup to usbdevice.cpp, IOCTLs to queue.cpp.
//
// API reference:
//   - Microsoft Windows-driver-samples/usb/UDE_*  (canonical UDE sample)
//   - https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/udecx/

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

    // Tell the framework we'll be a UDE controller. This must precede
    // WdfDeviceCreate.
    status = UdecxInitializeWdfDeviceInit(DeviceInit);
    if (!NT_SUCCESS(status)) return status;

    WDF_OBJECT_ATTRIBUTES deviceAttrs;
    WDF_OBJECT_ATTRIBUTES_INIT_CONTEXT_TYPE(&deviceAttrs, DEVICE_CONTEXT);

    WDFDEVICE device;
    status = WdfDeviceCreate(&DeviceInit, &deviceAttrs, &device);
    if (!NT_SUCCESS(status)) return status;

    // TODO: fill in UDECX_WDF_DEVICE_CONFIG (controller capabilities,
    // controller-state callbacks). See Microsoft UDE sample's equivalent
    // function for the right initializer pattern with current WDK.
    UDECX_WDF_DEVICE_CONFIG udeConfig;
    UDECX_WDF_DEVICE_CONFIG_INIT(&udeConfig, /* TODO: callbacks */ nullptr);

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

    // Plug the virtual Deck into our virtual host controller. Windows now
    // sees a USB device with VID 0x28de PID 0x1205 appear.
    return UsbDeviceCreate(device);
}
