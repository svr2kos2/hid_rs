async function send_reports(device, firmware) {
    let buffer = new ArrayBuffer(63);
    let view = new DataView(buffer);
    let hidCheckSum = firmware.at(firmware.length - 6);
    let encypt = firmware.at(firmware.length - 5);
    let writeDataCmd = firmware.at(firmware.length - 4);
    let sizeOfAddr = firmware.at(firmware.length - 3);
    let bigEndian = firmware.at(firmware.length - 2) == 0;
    let errForSize = firmware.at(firmware.length - 1);
    let firmwareSize = firmware.length - 6;
    console.log('hidCheckSum', hidCheckSum);
    console.log('encypt', encypt);
    console.log('writeDataCmd', writeDataCmd);
    console.log('sizeOfAddr', sizeOfAddr);
    console.log('bigEndian', bigEndian);
    console.log('errForSize', errForSize);
    console.log('firmwareSize', firmwareSize);

    view.setUint8(0, writeDataCmd);
    for (var addr = 0; addr < firmwareSize; addr += 32) {
        const response = new Promise((resolve) => {
            device.addEventListener('inputreport', resolve, { once: true });
        });
        var iBuffer = 1;
        let dataSize = Math.min(32, firmwareSize - addr);
        view.setUint8(iBuffer++, dataSize + sizeOfAddr + (encypt == 1 ? 1 : 0));
        if (sizeOfAddr == 2) {
            view.setUint16(iBuffer, addr, bigEndian);
            iBuffer += 2;
        } else {
            view.setUint32(iBuffer, addr, bigEndian);
            iBuffer += 4;
        }

        if (encypt == 1) {
            view.setUint8(iBuffer++, 0x01);
        }

        for (let i = 0; i < dataSize; i++) {
            view.setUint8(iBuffer + i, firmware[addr + i]);
        }
        iBuffer += dataSize;

        if (hidCheckSum != 0) {
            var sum = 2;
            for (let i = 0; i < iBuffer; i++) {
                sum += view.getUint8(i);
            }
            view.setUint8(iBuffer++, sum & 0xff);
        }
        
        device.sendReport(0x02, buffer);
        // console.log('Sent packet', [...new Uint8Array(buffer)]
        //     .map(x => x.toString(16).padStart(2, '0'))
        //     .join(' '));
        let res = await response;
        let statusIndex = (errForSize == 1) ? 2 : 1;
        if (res.data.getUint8(statusIndex) != 0x00) {
            wasm_bindgen.send_firmware_progress((addr / firmwareSize));
            console.log('Error state', res.data.getUint8(statusIndex));
            return false;
        }

        if ((addr / 32) % 100 == 0) {
            console.log('Sent', addr.toString());
            wasm_bindgen.send_firmware_progress((addr / firmwareSize));
        }
    }
    return true;
}
return send_reports(device, firmware);
