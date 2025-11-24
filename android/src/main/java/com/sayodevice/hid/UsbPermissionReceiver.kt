package com.sayodevice.hid

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.hardware.usb.UsbDevice
import android.hardware.usb.UsbManager

/**
 * Receives the explicit USB permission result and logs it via FlutterLogger.
 * The bridge polls UsbManager.hasPermission(), so this is informational only.
 */
class UsbPermissionReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        if (intent.action == "com.sayodevice.hid.USB_PERMISSION") {
            val device: UsbDevice? = if (android.os.Build.VERSION.SDK_INT >= 33) {
                intent.getParcelableExtra(UsbManager.EXTRA_DEVICE, UsbDevice::class.java)
            } else {
                @Suppress("DEPRECATION")
                intent.getParcelableExtra(UsbManager.EXTRA_DEVICE)
            }
            val granted: Boolean = intent.getBooleanExtra(UsbManager.EXTRA_PERMISSION_GRANTED, false)
            val devStr = if (device != null) "vid=${device.vendorId} pid=${device.productId} name=${device.deviceName}" else "<null>"
            if (granted) {
                FlutterLogger.i("UsbPermissionReceiver", "USB permission granted for $devStr")
            } else {
                FlutterLogger.w("UsbPermissionReceiver", "USB permission denied for $devStr")
            }
        }
    }
}
