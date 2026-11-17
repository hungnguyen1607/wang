//! Pinned downloads for local speech. Each is checked against its SHA-256 before use.
//!
//! Hashes: the two release archives were downloaded and hashed on 2026-11-16; model files
//! use the SHA-256 that Hugging Face publishes for each LFS object.

pub struct Asset {
    /// File name under the voice directory (archives are unpacked there).
    pub file: &'static str,
    pub url: &'static str,
    pub sha256: &'static str,
    pub bytes: u64,
}

/// whisper.cpp b5454 CPU build: `whisper-cli` + its libs (unpacks to `whisper-bin-ubuntu-x64/`).
pub const WHISPER_BIN: Asset = Asset {
    file: "whisper.tgz",
    url: "https://github.com/ggml-org/whisper.cpp/releases/download/b5454/whisper-bin-ubuntu-x64.tar.gz",
    sha256: "a72becf15d7917f990f6313867a52638b82b7f9ef237fb0c980dac56a135781c",
    bytes: 10_364_195,
};

/// Wake-word pass: fast, rough.
pub const WHISPER_TINY_EN: Asset = Asset {
    file: "ggml-tiny.en.bin",
    url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-tiny.en.bin",
    sha256: "921e4cf8686fdd993dcd081a5da5b6c365bfde1162e72b08d75ac75289920b1f",
    bytes: 77_704_715,
};

/// Command pass: what you actually said.
pub const WHISPER_BASE_EN: Asset = Asset {
    file: "ggml-base.en.bin",
    url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.en.bin",
    sha256: "a03779c86df3323075f5e796cb2ce5029f00ec8869eee3fdfb897afe36c6d002",
    bytes: 147_964_211,
};

/// Piper 2023.11.14-2, the last standalone Linux build (unpacks to `piper/`, espeak-ng data included).
pub const PIPER_BIN: Asset = Asset {
    file: "piper.tgz",
    url: "https://github.com/rhasspy/piper/releases/download/2023.11.14-2/piper_linux_x86_64.tar.gz",
    sha256: "a50cb45f355b7af1f6d758c1b360717877ba0a398cc8cbe6d2a7a3a26e225992",
    bytes: 26_460_462,
};

/// Default voice: en_GB "alan", medium quality (22.05 kHz).
pub const VOICE_ONNX: Asset = Asset {
    file: "voice.onnx",
    url: "https://huggingface.co/rhasspy/piper-voices/resolve/main/en/en_GB/alan/medium/en_GB-alan-medium.onnx",
    sha256: "0a309668932205e762801f1efc2736cd4b0120329622adf62be09e56339d3330",
    bytes: 63_201_294,
};

pub const VOICE_JSON: Asset = Asset {
    file: "voice.onnx.json",
    url: "https://huggingface.co/rhasspy/piper-voices/resolve/main/en/en_GB/alan/medium/en_GB-alan-medium.onnx.json",
    sha256: "c0f0d124e5895c00e7c03b35dcc8287f319a6998a365b182deb5c8e752ee8c1e",
    bytes: 4_888,
};

pub const ALL: [&Asset; 6] = [&WHISPER_BIN, &WHISPER_TINY_EN, &WHISPER_BASE_EN, &PIPER_BIN, &VOICE_ONNX, &VOICE_JSON];

pub fn total_bytes() -> u64 {
    ALL.iter().map(|a| a.bytes).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pins_look_right() {
        for a in ALL {
            assert_eq!(a.sha256.len(), 64, "{}", a.file);
            assert!(a.sha256.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()), "{}", a.file);
            assert!(a.url.starts_with("https://"), "{}", a.file);
        }
        assert!(total_bytes() > 300_000_000 && total_bytes() < 400_000_000);
    }
}
