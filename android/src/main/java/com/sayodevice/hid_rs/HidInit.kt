package com.sayodevice.hid_rs

import android.content.Context

object HidInit {
    // The native library is loaded by the consuming application (e.g. libapp.so in Tauri)
    // So we don't need to load "hid_rs" explicitly here.

    external fun initAndroidContext(context: Context)

    fun init(context: Context) {
        initAndroidContext(context)
    }
}
