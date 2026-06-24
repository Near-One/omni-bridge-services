use serde::{
    Deserialize,
    de::{self},
};

const MAX_PREFIX_LEN: usize = 32;

const MIN_SUPPORTED_HTTP_STATUS_CODE: u16 = 100;
const MAX_SUPPORTED_HTTP_STATUS_CODE: u16 = 599;

pub type RpcCode = i32;

#[derive(PartialEq, Eq, Hash, Clone, Debug)]
pub struct Prefix(String);

impl<'de> Deserialize<'de> for Prefix {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let prefix = String::deserialize(deserializer)?;
        if prefix.len() > MAX_PREFIX_LEN {
            return Err(de::Error::custom(format!(
                "prefix must be under {MAX_PREFIX_LEN} bytes"
            )));
        }
        if !prefix.starts_with('/') {
            return Err(de::Error::custom("prefix must start with `/`"));
        }
        if !prefix
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b == b'-' || b == b'/')
        {
            return Err(de::Error::custom(
                "prefix must contain only lowercase letters/dashes/slashes",
            ));
        }
        Ok(Prefix(prefix))
    }
}

impl Prefix {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(PartialEq, Eq, Debug)]
pub struct HttpStatusCode(u16);

impl<'de> Deserialize<'de> for HttpStatusCode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let status_code = <u16>::deserialize(deserializer)?;
        if !(MIN_SUPPORTED_HTTP_STATUS_CODE..=MAX_SUPPORTED_HTTP_STATUS_CODE).contains(&status_code)
        {
            return Err(de::Error::custom(format!(
                "status code should be in range {MIN_SUPPORTED_HTTP_STATUS_CODE}..={MAX_SUPPORTED_HTTP_STATUS_CODE}"
            )));
        }
        Ok(HttpStatusCode(status_code))
    }
}

impl HttpStatusCode {
    pub(crate) fn value(&self) -> u16 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_test::{Token, assert_de_tokens, assert_de_tokens_error};

    #[test]
    fn test_valid_prefixes() {
        let max_len: &'static str =
            Box::leak(format!("/{}", "a".repeat(MAX_PREFIX_LEN - 1)).into_boxed_str());
        for prefix in ["/eth", "/eth-beacon", "/ws/solana", max_len] {
            assert_de_tokens(&Prefix(prefix.to_string()), &[Token::Str(prefix)]);
        }
    }

    #[test]
    fn test_invalid_prefixes() {
        assert_de_tokens_error::<Prefix>(&[Token::Str("eth")], "prefix must start with `/`");

        for prefix in ["/ETH", "/Eth", "/eth1", "/eth!"] {
            assert_de_tokens_error::<Prefix>(
                &[Token::Str(prefix)],
                "prefix must contain only lowercase letters/dashes/slashes",
            );
        }

        assert_de_tokens_error::<Prefix>(
            &[Token::Str(Box::leak(
                "a".repeat(MAX_PREFIX_LEN + 1).into_boxed_str(),
            ))],
            &format!("prefix must be under {MAX_PREFIX_LEN} bytes"),
        );
    }

    #[test]
    fn test_valid_http_status_codes() {
        for http_status_code in MIN_SUPPORTED_HTTP_STATUS_CODE..=MAX_SUPPORTED_HTTP_STATUS_CODE {
            assert_de_tokens(
                &HttpStatusCode(http_status_code),
                &[Token::U16(http_status_code)],
            );
        }
    }

    #[test]
    fn test_invalid_http_status_codes() {
        assert_de_tokens_error::<HttpStatusCode>(
            &[Token::U16(
                MIN_SUPPORTED_HTTP_STATUS_CODE.checked_sub(1).unwrap(),
            )],
            &format!(
                "status code should be in range {MIN_SUPPORTED_HTTP_STATUS_CODE}..={MAX_SUPPORTED_HTTP_STATUS_CODE}"
            ),
        );
        assert_de_tokens_error::<HttpStatusCode>(
            &[Token::U16(
                MAX_SUPPORTED_HTTP_STATUS_CODE.checked_add(1).unwrap(),
            )],
            &format!(
                "status code should be in range {MIN_SUPPORTED_HTTP_STATUS_CODE}..={MAX_SUPPORTED_HTTP_STATUS_CODE}"
            ),
        );
    }
}
