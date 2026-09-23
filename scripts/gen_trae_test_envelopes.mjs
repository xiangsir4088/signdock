// 再生成 src-tauri/src/adapter/trae.rs 内嵌的三枚固定测试信封常量（纯本地假数据，无网络）。
// 用法（仓库根目录）：node scripts/gen_trae_test_envelopes.mjs
// 然后把打印出的常量粘贴进 trae.rs 的 tests 模块。
//
// 确定性说明（重要）：
//  1. 前两枚信封锚定在原始生成时刻：EPOCH_VALID = 2026-09-23T06:53:44.734Z（expiredAt=+30d），
//     EPOCH_EXPIRED 比其晚 4ms（expiredAt=-1d）。不用 Date.now()，保证任何时间重跑都与
//     trae.rs 已提交常量逐字节一致（已验证）。
//  2. AES-128-CBC 必须 setAutoPadding(false)：外层 PKCS7 已手工补齐，若开启 Node 默认自动填充
//     会再多出一整块 16 字节 → 信封变长且 Rust 侧完整性校验失败（Temp 版脚本曾有此缺陷）。
//
// 假数据约束：token 一律 TK-*，userId 固定 1234567890123456，embedded key 固定填充字节。
import crypto from "node:crypto";

const HEADER = Buffer.from([116, 99, 5, 16, 0, 0]);
const LEFT = Buffer.from([82,9,106,213,48,54,165,56,191,64,163,158,129,243,215,251,124,227,57,130,155,47,255,135,52,142,67,68,196,222,233,203,84,123,148,50,166,194,35,61,238,76,149,11,66,250,195,78,8,46,161,102,40,217,36,178,118,91,162,73,109,139,209,37]);
const RIGHT = Buffer.from([31,221,168,51,136,7,199,49,177,18,16,89,39,128,236,95,96,81,127,169,25,181,74,13,45,229,122,159,147,201,156,239,160,224,59,77,174,42,245,176,200,235,187,60,131,83,153,97,23,43,4,126,186,119,214,38,225,105,20,99,85,33,12,125]);
const sha512 = (b) => crypto.createHash("sha512").update(b).digest();

function encrypt(plain, embeddedKey) {
  const secret = Buffer.alloc(64);
  for (let i = 0; i < 64; i++) secret[i] = LEFT[i] ^ RIGHT[i];
  const derived = sha512(Buffer.concat([sha512(embeddedKey), secret]));
  const data = Buffer.concat([sha512(Buffer.from(plain, "utf8")), Buffer.from(plain, "utf8")]);
  const pad = 16 - (data.length % 16);
  const padded = Buffer.concat([data, Buffer.alloc(pad, pad)]);
  const c = crypto.createCipheriv("aes-128-cbc", derived.subarray(0, 16), derived.subarray(16, 32));
  c.setAutoPadding(false); // 见文件头说明 2
  const ct = Buffer.concat([c.update(padded), c.final()]);
  return Buffer.concat([HEADER, embeddedKey, ct]).toString("base64");
}

const day = 86400000;
const EPOCH_VALID = Date.parse("2026-10-23T06:53:44.734Z") - 30 * day; // 原始生成时刻
const EPOCH_EXPIRED = EPOCH_VALID + 4;
const iso = (ms) => new Date(ms).toISOString();
const base = (now, exp) => ({
  token: "TK-" + (exp > now ? "VALID" : "EXPIRED"),
  refreshToken: "RT-TEST",
  userId: "1234567890123456",
  host: "https://api.trae.cn",
  expiredAt: iso(exp),
  refreshExpiredAt: iso(exp + 365 * day),
  account: { username: "tester" },
  userRegion: { region: "CN" },
});

const keyA = Buffer.alloc(32, 7);
const keyB = Buffer.alloc(32, 9);
const keyC = Buffer.alloc(32, 0x0b);

// 缺 token 的 payload：与 VALID 同构但无 token 字段；expiredAt 远期未来（解出即定，缺 token 直接失败）
const noToken = base(EPOCH_VALID, Date.parse("2100-01-01T00:00:00.000Z"));
delete noToken.token;

console.log("VALID(30d) payload:", JSON.stringify(base(EPOCH_VALID, EPOCH_VALID + 30 * day)));
console.log("ENVELOPE_VALID =", JSON.stringify(encrypt(JSON.stringify(base(EPOCH_VALID, EPOCH_VALID + 30 * day)), keyA)));
console.log("\nEXPIRED payload:", JSON.stringify(base(EPOCH_EXPIRED, EPOCH_EXPIRED - day)));
console.log("ENVELOPE_EXPIRED =", JSON.stringify(encrypt(JSON.stringify(base(EPOCH_EXPIRED, EPOCH_EXPIRED - day)), keyB)));
console.log("\nNO_TOKEN payload:", JSON.stringify(noToken));
console.log("ENVELOPE_NO_TOKEN =", JSON.stringify(encrypt(JSON.stringify(noToken), keyC)));
