//! What iroh's network report says about the link, and when a change of it is
//! believed.
//!
//! iroh 0.97 has no public link-change event. Its network report changes when
//! the UDP families that work or the public addresses change. A report that
//! flaps (a probe that failed once) must not be believed: a new print counts
//! only when two reports in a row carry it ([`Debounce`]).

use std::net::IpAddr;

/// UDP over IPv4 works, UDP over IPv6 works, the public IPv4 and IPv6 address.
pub(crate) type LinkPrint = (bool, bool, Option<IpAddr>, Option<IpAddr>);

#[cfg(feature = "link-watch")]
pub(crate) fn print_of(r: &iroh::NetReport) -> LinkPrint {
    (
        r.udp_v4,
        r.udp_v6,
        r.global_v4.map(|a| IpAddr::V4(*a.ip())),
        r.global_v6.map(|a| IpAddr::V6(*a.ip())),
    )
}

/// Believes a new print once two reports in a row carry it. The first report
/// is the baseline.
#[derive(Debug, Default)]
pub(crate) struct Debounce {
    stable: Option<LinkPrint>,
    pending: Option<(LinkPrint, u8)>,
}

impl Debounce {
    /// Feed one report. `true`: the link changed, and this is the second
    /// report in a row that says so.
    pub(crate) fn feed(&mut self, print: LinkPrint) -> bool {
        match self.stable {
            None => {
                self.stable = Some(print);
                false
            }
            Some(stable) if stable == print => {
                self.pending = None;
                false
            }
            Some(_) => {
                let seen = match self.pending {
                    Some((p, n)) if p == print => n + 1,
                    _ => 1,
                };
                if seen >= 2 {
                    self.stable = Some(print);
                    self.pending = None;
                    true
                } else {
                    self.pending = Some((print, seen));
                    false
                }
            }
        }
    }

    /// Forget everything: the next report is a baseline (a new endpoint).
    #[cfg_attr(not(feature = "link-watch"), allow(dead_code))]
    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(v6: bool, ip: u8) -> LinkPrint {
        (true, v6, Some(IpAddr::from([203, 0, 113, ip])), None)
    }

    #[test]
    fn the_first_report_is_a_baseline() {
        let mut d = Debounce::default();
        assert!(!d.feed(p(false, 1)));
        assert!(!d.feed(p(false, 1)));
    }

    #[test]
    fn a_new_print_counts_at_its_second_report_in_a_row() {
        let mut d = Debounce::default();
        d.feed(p(false, 1));
        assert!(!d.feed(p(false, 2)));
        assert!(d.feed(p(false, 2)));
        // It is the baseline now.
        assert!(!d.feed(p(false, 2)));
    }

    #[test]
    fn a_flapping_sequence_never_counts() {
        let mut d = Debounce::default();
        d.feed(p(true, 1));
        for _ in 0..50 {
            assert!(!d.feed(p(false, 1)));
            assert!(!d.feed(p(true, 1)));
        }
    }

    #[test]
    fn two_different_new_prints_in_a_row_do_not_add_up() {
        let mut d = Debounce::default();
        d.feed(p(false, 1));
        assert!(!d.feed(p(false, 2)));
        assert!(!d.feed(p(false, 3)));
        assert!(d.feed(p(false, 3)));
    }

    #[test]
    fn a_reset_makes_the_next_report_a_baseline() {
        let mut d = Debounce::default();
        d.feed(p(false, 1));
        d.reset();
        assert!(!d.feed(p(false, 2)));
        assert!(!d.feed(p(false, 2)));
    }
}
