use ipnetwork::IpNetwork;
use rfc7239::{parse, Forwarded, NodeIdentifier, NodeName};
use std::borrow::Cow;
use std::convert::Infallible;
use std::iter::{once, FromIterator, IntoIterator};
use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;
use warp::filters::addr::remote;
use warp::Filter;

/// Represents a set of IP networks.
#[derive(Debug, Clone)]
pub struct IpNetworks {
    networks: Vec<IpNetwork>,
}

impl IpNetworks {
    /// Checks if addr is part of any IP networks included.
    pub fn contains(&self, addr: &IpAddr) -> bool {
        self.networks.iter().any(|&network| network.contains(*addr))
    }
}

impl From<Vec<IpAddr>> for IpNetworks {
    fn from(addrs: Vec<IpAddr>) -> Self {
        addrs.into_iter().collect()
    }
}

impl From<&[IpAddr]> for IpNetworks {
    fn from(addrs: &[IpAddr]) -> Self {
        addrs.iter().copied().collect()
    }
}

impl FromIterator<IpAddr> for IpNetworks {
    fn from_iter<T: IntoIterator<Item = IpAddr>>(addrs: T) -> Self {
        addrs.into_iter().map(IpNetwork::from).collect()
    }
}

impl FromIterator<IpNetwork> for IpNetworks {
    fn from_iter<T: IntoIterator<Item = IpNetwork>>(addrs: T) -> Self {
        IpNetworks {
            networks: addrs.into_iter().collect(),
        }
    }
}

/// Creates a `Filter` that provides the "real ip" of the connected client.
///
/// This uses the "x-forwarded-for" or "x-real-ip" headers set by reverse proxies.
/// To stop clients from abusing these headers, only headers set by trusted remotes will be accepted.
///
/// Note that if multiple forwarded-for addresses are present, which can be the case when using nested reverse proxies,
/// all proxies in the chain have to be within the list of trusted proxies.
///
/// ## Example
///
/// ```no_run
/// use warp::Filter;
/// use warp_real_ip::real_ip;
/// use std::net::IpAddr;
///
/// let proxy_addr = [127, 10, 0, 1].into();
/// warp::any()
///     .and(real_ip(vec![proxy_addr]))
///     .map(|addr: Option<IpAddr>| format!("Hello {}", addr.unwrap()));
/// ```
pub fn real_ip(
    trusted_proxies: impl Into<IpNetworks>,
) -> impl Filter<Extract = (Option<IpAddr>,), Error = Infallible> + Clone {
    let trusted_proxies = trusted_proxies.into();
    remote().and(get_forwarded_for()).map(
        move |addr: Option<SocketAddr>, forwarded_for: Vec<IpAddr>| {
            addr.map(|addr| {
                let hops = forwarded_for.iter().copied().chain(once(addr.ip()));
                for hop in hops {
                    if !trusted_proxies.contains(&hop) {
                        return hop;
                    }
                }

                // all hops were trusted, return the last one
                forwarded_for.first().copied().unwrap_or_else(|| addr.ip())
            })
        },
    )
}

/// Creates a `Filter` that extracts the ip addresses from the the "forwarded for" chain
pub fn get_forwarded_for() -> impl Filter<Extract = (Vec<IpAddr>,), Error = Infallible> + Clone {
    warp::header("x-forwarded-for")
        .map(|list: CommaSeparated<IpAddr>| list.into_inner())
        .or(warp::header("x-real-ip").map(|ip: String| {
            IpAddr::from_str(maybe_bracketed(&maybe_quoted(&ip)))
                .map_or_else(|_| Vec::<IpAddr>::new(), |x| vec![x])
        }))
        .unify()
        .or(warp::header("forwarded").map(|header: String| {
            parse(&header)
                .filter_map(|forward| match forward {
                    Ok(Forwarded {
                        forwarded_for:
                            Some(NodeIdentifier {
                                name: NodeName::Ip(ip),
                                ..
                            }),
                        ..
                    }) => Some(ip),
                    _ => None,
                })
                .collect::<Vec<_>>()
        }))
        .unify()
        .or(warp::any().map(Vec::new))
        .unify()
}

#[derive(Copy, Clone)]
enum CommaSeparatedIteratorState {
    /// Start of string or after a ',' (including whitespace)
    Default,
    /// Inside a double quote
    Quoted,
    /// After escape character inside quote
    QuotedPair,
    /// Non quoted part
    Token,
    /// After closing double quote
    PostAmbleForQuoted,
}

struct CommaSeparatedIterator<'a> {
    /// target
    target: &'a str,
    /// iterator
    char_indices: std::str::CharIndices<'a>,
    /// current scanner state
    state: CommaSeparatedIteratorState,
    /// start position of the last token found
    s: usize,
}

impl<'a> CommaSeparatedIterator<'a> {
    pub fn new(target: &'a str) -> Self {
        Self {
            target,
            char_indices: target.char_indices(),
            state: CommaSeparatedIteratorState::Default,
            s: 0,
        }
    }
}

impl<'a> Iterator for CommaSeparatedIterator<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<Self::Item> {
        for (i, c) in &mut self.char_indices {
            let (next, next_state) = match (self.state, c) {
                (CommaSeparatedIteratorState::Default, '"') => {
                    self.s = i;
                    (None, CommaSeparatedIteratorState::Quoted)
                }
                (CommaSeparatedIteratorState::Default, ' ' | '\t') => {
                    (None, CommaSeparatedIteratorState::Default)
                }
                (CommaSeparatedIteratorState::Default, ',') => (
                    Some(Some(&self.target[i..i])),
                    CommaSeparatedIteratorState::Default,
                ),
                (CommaSeparatedIteratorState::Default, _) => {
                    self.s = i;
                    (None, CommaSeparatedIteratorState::Token)
                }
                (CommaSeparatedIteratorState::Quoted, '"') => (
                    Some(Some(&self.target[self.s..i + 1])),
                    CommaSeparatedIteratorState::PostAmbleForQuoted,
                ),
                (CommaSeparatedIteratorState::Quoted, '\\') => {
                    (None, CommaSeparatedIteratorState::QuotedPair)
                }
                (CommaSeparatedIteratorState::QuotedPair, _) => {
                    (None, CommaSeparatedIteratorState::Quoted)
                }
                (CommaSeparatedIteratorState::Token, ',') => (
                    Some(Some(&self.target[self.s..i])),
                    CommaSeparatedIteratorState::Default,
                ),
                (CommaSeparatedIteratorState::PostAmbleForQuoted, ',') => {
                    (None, CommaSeparatedIteratorState::Default)
                }
                (current_state, _) => (None, current_state),
            };
            self.state = next_state;
            if let Some(next) = next {
                return next;
            }
        }
        match self.state {
            CommaSeparatedIteratorState::Default
            | CommaSeparatedIteratorState::PostAmbleForQuoted => None,
            CommaSeparatedIteratorState::Quoted
            | CommaSeparatedIteratorState::QuotedPair
            | CommaSeparatedIteratorState::Token => {
                self.state = CommaSeparatedIteratorState::Default;
                Some(&self.target[self.s..])
            }
        }
    }
}

enum EscapeState {
    Normal,
    Escaped,
}

fn maybe_quoted(x: &str) -> Cow<str> {
    let mut i = x.chars();
    if i.next() == Some('"') {
        let mut s = String::with_capacity(x.len());
        let mut state = EscapeState::Normal;
        for c in i {
            state = match state {
                EscapeState::Normal => match c {
                    '"' => break,
                    '\\' => EscapeState::Escaped,
                    _ => {
                        s.push(c);
                        EscapeState::Normal
                    }
                },
                EscapeState::Escaped => {
                    s.push(c);
                    EscapeState::Normal
                }
            };
        }
        s.into()
    } else {
        x.into()
    }
}

fn maybe_bracketed(x: &str) -> &str {
    if x.as_bytes().first() == Some(&b'[') && x.as_bytes().last() == Some(&b']') {
        &x[1..x.len() - 1]
    } else {
        x
    }
}

/// Newtype so we can implement FromStr
struct CommaSeparated<T>(Vec<T>);

impl<T> CommaSeparated<T> {
    pub fn into_inner(self) -> Vec<T> {
        self.0
    }
}

impl<T: FromStr> FromStr for CommaSeparated<T> {
    type Err = T::Err;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let vec = CommaSeparatedIterator::new(s)
            .map(|x| T::from_str(maybe_bracketed(&maybe_quoted(x.trim()))))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(CommaSeparated(vec))
    }
}

#[cfg(test)]
mod tests {
    use crate::{maybe_bracketed, maybe_quoted, CommaSeparatedIterator};

    #[test]
    fn test_comma_separated_iterator() {
        assert_eq!(
            vec!["abc", "def", "ghi", "jkl ", "mno", "pqr"],
            CommaSeparatedIterator::new("abc,def, ghi,\tjkl , mno,\tpqr").collect::<Vec<&str>>()
        );
        assert_eq!(
            vec![
                "abc",
                "\"def\"",
                "\"ghi\"",
                "\"jkl\"",
                "\"mno\"",
                "pqr",
                "\"abc, def\"",
            ],
            CommaSeparatedIterator::new(
                "abc,\"def\", \"ghi\",\t\"jkl\" , \"mno\",\tpqr, \"abc, def\""
            )
            .collect::<Vec<&str>>()
        );
    }

    #[test]
    fn test_maybe_quoted() {
        assert_eq!("abc", maybe_quoted("abc"));
        assert_eq!("abc", maybe_quoted("\"abc\""));
        assert_eq!("a\"bc", maybe_quoted("\"a\\\"bc\""));
    }

    #[test]
    fn test_maybe_bracketed() {
        assert_eq!("abc", maybe_bracketed("abc"));
        assert_eq!("abc", maybe_bracketed("[abc]"));
        assert_eq!("[abc", maybe_bracketed("[abc"));
        assert_eq!("abc]", maybe_bracketed("abc]"));
    }
}
