use crate::archive::ChunkSink;
use age::secrecy::SecretString;
use std::io;

/// 用 age 口令模式包装写入器：数据先压缩后进这里，再流向 ChunkSink
pub fn encrypt(
    sink: ChunkSink,
    passphrase: &str,
) -> io::Result<age::stream::StreamWriter<ChunkSink>> {
    let passphrase = SecretString::from(passphrase.to_string());
    age::Encryptor::with_user_passphrase(passphrase).wrap_output(sink)
}
