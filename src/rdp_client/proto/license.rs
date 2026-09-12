//! The licensing exchange, which for this client is one PDU that says there is none.
//!
//! A Windows host answers the Client Info PDU with a licensing PDU before it will
//! start a share. Against a Remote Desktop Session Host with per-device licensing
//! that is the start of a real exchange — a licence request, a platform challenge, a
//! certificate — but against the hosts this client targets it is a single message
//! meaning *this client needs no licence*: an error alert whose error code is
//! `STATUS_VALID_CLIENT`.
//!
//! So there is nothing here to implement, only something to recognize, and one thing
//! to refuse clearly. A server that opens a real licensing exchange gets a sentence
//! naming what it asked for rather than a decoder walking into a certificate it was
//! never going to be able to answer.

use super::share;
use super::wire::{Malformed, Reader};

/// `ERROR_ALERT`. The other preamble types are all steps of an exchange this client
/// does not take part in, so they are named where they are refused rather than here.
const ERROR_ALERT: u8 = 0xFF;

/// `STATUS_VALID_CLIENT`: no licence is needed, carry on to the share.
const STATUS_VALID_CLIENT: u32 = 0x0000_0007;

/// Read the server's licensing PDU, and accept only the one that means the session
/// may continue.
pub fn accept(payload: &[u8]) -> Result<(), Malformed> {
    const WHAT: &str = "a licensing PDU";

    let mut r = Reader::new(WHAT, payload);
    let flags = share::read_security_header(&mut r)?;
    if flags & share::LICENSE_PACKET == 0 {
        return Err(r.refuse("its security flags", flags));
    }

    let preamble = r.u8()?;
    if preamble != ERROR_ALERT {
        return Err(r.refuse("its message type", preamble));
    }
    // The flags and version of the preamble, and the message size after it: nothing
    // here changes what the one message this client accepts means.
    r.skip(3)?;

    let error = r.u32_le()?;
    if error != STATUS_VALID_CLIENT {
        return Err(r.refuse("its error code", error));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole PDU a Windows host sends: the security header, the preamble, the
    /// error code, the state transition, and an empty error blob.
    fn valid_client() -> Vec<u8> {
        vec![
            0x80, 0x00, 0x00, 0x00, // SEC_LICENSE_PKT
            0xFF, 0x03, 0x10, 0x00, // ERROR_ALERT, version 3, sixteen bytes
            0x07, 0x00, 0x00, 0x00, // STATUS_VALID_CLIENT
            0x02, 0x00, 0x00, 0x00, // ST_NO_TRANSITION
            0x04, 0x00, 0x00, 0x00, // an error blob of nothing
        ]
    }

    #[test]
    fn the_answer_that_means_no_licence_is_needed_is_accepted() {
        assert_eq!(accept(&valid_client()), Ok(()));
    }

    #[test]
    fn a_host_that_wants_a_licence_says_which_message_it_sent() {
        // A Server License Request, which is where a per-device exchange starts.
        let mut pdu = valid_client();
        pdu[4] = 0x01;
        assert_eq!(
            accept(&pdu).unwrap_err().to_string(),
            "a licensing PDU carries its message type 0x1, which this client does not accept"
        );
    }

    #[test]
    fn an_error_alert_that_is_a_real_error_is_not_read_as_success() {
        // ERR_NO_LICENSE_SERVER.
        let mut pdu = valid_client();
        pdu[8] = 0x06;
        assert_eq!(
            accept(&pdu).unwrap_err().to_string(),
            "a licensing PDU carries its error code 0x6, which this client does not accept"
        );
    }

    #[test]
    fn a_pdu_that_is_not_part_of_the_licensing_exchange_is_refused_at_its_header() {
        // A share control header, which is what arrives if the exchange is already
        // over and this is being read a PDU too late.
        let pdu = [0x1A, 0x01, 0x11, 0x00, 0xEA, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        assert_eq!(
            accept(&pdu).unwrap_err().to_string(),
            "a licensing PDU carries its security flags 0x11a, which this client does not accept"
        );
    }
}
