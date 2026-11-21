(() => {
  const dispatch = globalThis.parkHostCall;
  const encode = globalThis.parkBase64Encode;
  const decode = globalThis.parkBase64Decode;
  const decodeText = globalThis.parkDecodeText;
  const labelFor = globalThis.parkEncoding;
  const random = globalThis.parkRandom;
  const monotonic = globalThis.parkMonotonic;
  delete globalThis.parkHostCall;
  delete globalThis.parkBase64Encode;
  delete globalThis.parkBase64Decode;
  delete globalThis.parkDecodeText;
  delete globalThis.parkEncoding;
  delete globalThis.parkRandom;
  delete globalThis.parkMonotonic;
  async function bridge(operation, args) {
    const result = JSON.parse(dispatch(operation, JSON.stringify(args)));
    if (!result.ok) throw Object.assign(new Error(result.code), {code: result.code});
    return result.value;
  }
  globalThis.Park = {__native: {
    request: options => bridge('request', options),
    chatRequest: options => bridge('chatRequest', options),
    crypto: (op, args) => bridge('crypto', {op,args}),
    cookies: (op, args) => bridge('cookies', {op,args}),
    persist: (_key, data, flush) => bridge('persist', {data,flush}),
    log() {}
  }};
  globalThis.atob = text => {
    const value = decode(String(text).replace(/[\t\n\f\r ]/g,''));
    if (value == null) throw new TypeError('Invalid base64');
    return value;
  };
  globalThis.btoa = text => {
    const value = encode(String(text));
    if (value == null) throw new TypeError('Invalid byte string');
    return value;
  };
  function bytesOf(input) {
    return ArrayBuffer.isView(input)
      ? new Uint8Array(input.buffer, input.byteOffset, input.byteLength)
      : new Uint8Array(input);
  }
  globalThis.TextEncoder = class TextEncoder {
    get encoding() { return 'utf-8'; }
    encode(text = '') {
      const encoded = unescape(encodeURIComponent(String(text).toWellFormed()));
      return Uint8Array.from(encoded, c => c.charCodeAt(0));
    }
  };
  globalThis.TextDecoder = class TextDecoder {
    constructor(label = 'utf-8', options = {}) {
      this.encoding = labelFor(String(label));
      if (!this.encoding) throw new RangeError('Unsupported encoding');
      this.fatal = Boolean(options.fatal);
      this.ignoreBOM = Boolean(options.ignoreBOM);
    }
    decode(input) {
      const bytes = bytesOf(input);
      let latin = '';
      for (let i = 0; i < bytes.length; i += 0x8000) {
        latin += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
      }
      const result = decodeText(this.encoding, encode(latin), this.fatal, this.ignoreBOM);
      if (result == null) throw new TypeError('Invalid encoded text');
      return result;
    }
  };
  globalThis.crypto = Object.freeze({
    getRandomValues(view) {
      if (!ArrayBuffer.isView(view) || view instanceof DataView || view instanceof Float32Array
          || view instanceof Float64Array || view.byteLength > 65536) throw new TypeError('Invalid typed array');
      bytesOf(view).set(Uint8Array.from(decode(random(view.byteLength)), c => c.charCodeAt(0)));
      return view;
    },
    randomUUID() {
      const bytes = this.getRandomValues(new Uint8Array(16));
      bytes[6] = (bytes[6] & 15) | 64;
      bytes[8] = (bytes[8] & 63) | 128;
      const hex = Array.from(bytes, b => b.toString(16).padStart(2, '0')).join('');
      return [hex.slice(0,8),hex.slice(8,12),hex.slice(12,16),hex.slice(16,20),hex.slice(20)].join('-');
    }
  });
  globalThis.console = Object.freeze({log(){},error(){},warn(){},info(){}});
  const timers = new Map();
  let sequence = 0;
  globalThis.setTimeout = (callback, delay, ...args) => {
    if (typeof callback !== 'function') throw new TypeError('Invalid timer');
    if (timers.size >= 1024) throw new RangeError('Too many timers');
    const id = ++sequence;
    timers.set(id, {at: monotonic() + Math.max(0, Number(delay) || 0), callback, args});
    return id;
  };
  globalThis.clearTimeout = id => timers.delete(id);
  return (reset) => {
    if (reset) { timers.clear(); return -1; }
    let delay = Infinity;
    const now = monotonic();
    for (const [id, timer] of timers) {
      if (timer.at <= now) {
        timers.delete(id);
        timer.callback(...timer.args);
        return 0;
      }
      delay = Math.min(delay, timer.at - now);
    }
    return Number.isFinite(delay) ? Math.min(Math.ceil(delay), 10) : -1;
  };
})();
