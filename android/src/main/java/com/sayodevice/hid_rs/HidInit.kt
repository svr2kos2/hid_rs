package com.sayodevice.hid_rs

import android.content.Context

object HidInit {
    init {
        System.loadLibrary("hid_rs")
    }

    external fun initAndroidContext(context: Context)

    fun init(context: Context) {
        initAndroidContext(context)
    }
}
