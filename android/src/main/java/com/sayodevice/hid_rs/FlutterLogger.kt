package com.sayodevice.hid_rs

import android.util.Log

object FlutterLogger {
    // No initialization needed for standard Android logging

    fun log(level: String, tag: String, message: String) {
        try {
            when (level.lowercase()) {
                "e" -> Log.e(tag, message)
                "w" -> Log.w(tag, message)
                "i" -> Log.i(tag, message)
                "d" -> Log.d(tag, message)
                else -> Log.v(tag, message)
            }
        } catch (_: Throwable) {}
    }

    fun d(tag: String, message: String) = log("d", tag, message)
    fun i(tag: String, message: String) = log("i", tag, message)
    fun w(tag: String, message: String) = log("w", tag, message)
    fun e(tag: String, message: String) = log("e", tag, message)
    fun v(tag: String, message: String) = log("v", tag, message)
}
