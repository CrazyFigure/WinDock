import { createHash, createPrivateKey, createPublicKey, sign, verify } from 'node:crypto';
import { createReadStream } from 'node:fs';
import { readFile, writeFile } from 'node:fs/promises';
import { basename } from 'node:path';

const [asset, publicKeyPath, mode] = process.argv.slice(2);
if (!asset || !publicKeyPath || (mode !== undefined && mode !== '--verify')) {
  throw new Error('用法：node scripts/sign-update.mjs <安装包> <公钥文件> [--verify]');
}

// 签名绑定文件名与流式计算出的 SHA-256，消息格式与 Rust 更新器的 v2 协议一致。
const hash = createHash('sha256');
for await (const chunk of createReadStream(asset)) {
  hash.update(chunk);
}
const data = Buffer.concat([Buffer.from(`WinDock update v2\0${basename(asset)}\0`), hash.digest()]);
const expected = (await readFile(publicKeyPath, 'utf8')).trim().toLowerCase();
if (!/^[0-9a-f]{64}$/.test(expected)) throw new Error('更新公钥不是有效的 Ed25519 公钥');

if (mode === '--verify') {
  // 使用标签源码内置的公钥验证下载的附件，不需要访问签名私钥。
  const x = Buffer.from(expected, 'hex').toString('base64url');
  const key = createPublicKey({ key: { kty: 'OKP', crv: 'Ed25519', x }, format: 'jwk' });
  if (!verify(null, data, key, await readFile(`${asset}.sig`))) throw new Error('安装包签名无效');
} else {
  const privateKey = process.env.WINDOCK_UPDATE_PRIVATE_KEY;
  if (!privateKey) throw new Error('缺少 WINDOCK_UPDATE_PRIVATE_KEY');
  const key = createPrivateKey({ key: Buffer.from(privateKey.trim(), 'base64'), format: 'der', type: 'pkcs8' });
  const actual = Buffer.from(createPublicKey(key).export({ format: 'jwk' }).x, 'base64url').toString('hex');
  if (key.asymmetricKeyType !== 'ed25519' || actual !== expected) {
    throw new Error('更新签名密钥与客户端公钥不匹配');
  }
  await writeFile(`${asset}.sig`, sign(null, data, key));
}
