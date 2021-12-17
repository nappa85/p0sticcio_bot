use ingress_intel_rs::plexts::Markup;

use rust_decimal::Decimal;

use tracing::error;

use crate::dedup_flatten::DedupFlatten;

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum PlextType {
    Captured,
    CreatedCF,
    DestroyedCF,
    DeployedReso,
    DestroyedReso,
    DestroyedLink,
    Linked,
    DroneReturn,
    DeployedBeacon,
    DeployedFireworks,
    Unknown,
}

impl<'a> From<&'a [Markup]> for PlextType {
    fn from(markups: &'a [Markup]) -> Self {
        markups
            .iter()
            .find_map(|(_, markup)| match markup.plain.as_str() {
                " captured " => Some(PlextType::Captured),
                " created a Control Field @" => Some(PlextType::CreatedCF),
                " destroyed a Control Field @" => Some(PlextType::DestroyedCF),
                " deployed a Resonator on " => Some(PlextType::DeployedReso),
                " destroyed a Resonator on " => Some(PlextType::DestroyedReso),
                " destroyed the Link " => Some(PlextType::DestroyedLink),
                " linked " => Some(PlextType::Linked),
                "Drone returned to Agent by " => Some(PlextType::DroneReturn),
                " deployed a Beacon on " => Some(PlextType::DeployedBeacon),
                " deployed Fireworks on " => Some(PlextType::DeployedFireworks),
                _ => None,
            })
            .unwrap_or(PlextType::Unknown)
    }
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum Plext<'a> {
    /// [["PLAYER",{"plain":"bicilindrico","team":"RESISTANCE"}],["TEXT",{"plain":" captured "}],["PORTAL",{"plain":"Oratorio Di Villa Minelli (Via Roma, 136, 31050 Ponzano TV, Italy)","name":"Oratorio Di Villa Minelli","address":"Via Roma, 136, 31050 Ponzano TV, Italy","latE6":45708709,"lngE6":12217027,"team":"RESISTANCE"}]]
    Captured {
        player: Player<'a>,
        portal: Portal<'a>,
    },
    /// [["PLAYER",{"plain":"bicilindrico","team":"RESISTANCE"}],["TEXT",{"plain":" created a Control Field @"}],["PORTAL",{"plain":"Villorba-Capitello della Beata Vergine Maria (Via Campagnola, 72B, 31020 Villorba, Treviso, Italy)","name":"Villorba-Capitello della Beata Vergine Maria","address":"Via Campagnola, 72B, 31020 Villorba, Treviso, Italy","latE6":45759495,"lngE6":12230196,"team":"RESISTANCE"}],["TEXT",{"plain":" +"}],["TEXT",{"plain":"204"}],["TEXT",{"plain":" MUs"}]]
    CreatedCF {
        player: Player<'a>,
        portal: Portal<'a>,
        mu: usize,
    },
    /// [["PLAYER",{"plain":"signoreoscuro89","team":"RESISTANCE"}],["TEXT",{"plain":" destroyed a Control Field @"}],["PORTAL",{"plain":"Campo da Calcio in Erba Sintetica - Impianti Sportivi Ceron (Via Trasimeno, 8, 35030 Selvazzano Dentro PD, Italy)","name":"Campo da Calcio in Erba Sintetica - Impianti Sportivi Ceron","address":"Via Trasimeno, 8, 35030 Selvazzano Dentro PD, Italy","latE6":45389869,"lngE6":11798346,"team":"RESISTANCE"}],["TEXT",{"plain":" -"}],["TEXT",{"plain":"1620"}],["TEXT",{"plain":" MUs"}]]
    DestroyedCF {
        player: Player<'a>,
        portal: Portal<'a>,
        mu: usize,
    },
    /// [["PLAYER",{"plain":"bicilindrico","team":"RESISTANCE"}],["TEXT",{"plain":" deployed a Resonator on "}],["PORTAL",{"plain":"Oratorio Di Villa Minelli (Via Roma, 136, 31050 Ponzano TV, Italy)","name":"Oratorio Di Villa Minelli","address":"Via Roma, 136, 31050 Ponzano TV, Italy","latE6":45708709,"lngE6":12217027,"team":"RESISTANCE"}]]
    DeployedReso {
        player: Player<'a>,
        portal: Portal<'a>,
    },
    /// [["PLAYER",{"plain":"bicilindrico","team":"RESISTANCE"}],["TEXT",{"plain":" destroyed a Resonator on "}],["PORTAL",{"plain":"Parco Giochi \"saltimbanchi\" - Villorba (Via Po, 22, 31020 Lancenigo TV, Italy)","name":"Parco Giochi \"saltimbanchi\" - Villorba","address":"Via Po, 22, 31020 Lancenigo TV, Italy","latE6":45708103,"lngE6":12244696,"team":"ENLIGHTENED"}]]
    DestroyedReso {
        player: Player<'a>,
        portal: Portal<'a>,
    },
    /// [["PLAYER",{"plain":"signoreoscuro89","team":"RESISTANCE"}],["TEXT",{"plain":" destroyed the Link "}],["PORTAL",{"plain":"Campo da Calcio in Erba Sintetica - Impianti Sportivi Ceron (Via Trasimeno, 8, 35030 Selvazzano Dentro PD, Italy)","name":"Campo da Calcio in Erba Sintetica - Impianti Sportivi Ceron","address":"Via Trasimeno, 8, 35030 Selvazzano Dentro PD, Italy","latE6":45389869,"lngE6":11798346,"team":"ENLIGHTENED"}],["TEXT",{"plain":" to "}],["PORTAL",{"plain":"Monumento Agli Alpini (Via IV Novembre, 575, 35035 Mestrino PD, Italy)","name":"Monumento Agli Alpini","address":"Via IV Novembre, 575, 35035 Mestrino PD, Italy","latE6":45443431,"lngE6":11756890,"team":"RESISTANCE"}]]
    DestroyedLink {
        player: Player<'a>,
        source: Portal<'a>,
        target: Portal<'a>,
    },
    /// [["PLAYER",{"plain":"bicilindrico","team":"RESISTANCE"}],["TEXT",{"plain":" linked "}],["PORTAL",{"plain":"Villorba-Capitello della Beata Vergine Maria (Via Campagnola, 72B, 31020 Villorba, Treviso, Italy)","name":"Villorba-Capitello della Beata Vergine Maria","address":"Via Campagnola, 72B, 31020 Villorba, Treviso, Italy","latE6":45759495,"lngE6":12230196,"team":"RESISTANCE"}],["TEXT",{"plain":" to "}],["PORTAL",{"plain":"Santandra Capitello della Sacra Famiglia (Via dei Caduti, 3, 31050 Santandr\u00e0 Treviso, Italy)","name":"Santandra Capitello della Sacra Famiglia","address":"Via dei Caduti, 3, 31050 Santandr\u00e0 Treviso, Italy","latE6":45746774,"lngE6":12199663,"team":"RESISTANCE"}]]
    Linked {
        player: Player<'a>,
        source: Portal<'a>,
        target: Portal<'a>,
    },
    /// [["TEXT",{"plain":"Drone returned to Agent by "}],["PLAYER",{"plain":"bicilindrico","team":"RESISTANCE"}]]
    DroneReturn {
        player: Player<'a>,
    },
    /// [["PLAYER",{"plain":"AmmaNommo","team":"RESISTANCE"}],["TEXT",{"plain":" deployed a Beacon on "}],["PORTAL",{"plain":"Mestre - Biglietteria Stazione FS (Viale Stazione, 10, 30171 Venice, Italy)","name":"Mestre - Biglietteria Stazione FS","address":"Viale Stazione, 10, 30171 Venice, Italy","latE6":45482653,"lngE6":12231524,"team":"RESISTANCE"}]]
    DeployedBeacon {
        player: Player<'a>,
        portal: Portal<'a>,
    },
    /// [["PLAYER",{"plain":"bicilindrico","team":"RESISTANCE"}],["TEXT",{"plain":" deployed Fireworks on "}],["PORTAL",{"plain":"Palazzo Bianco Con Affresco - Tv (Via Sant'Agostino, 27, 31100 Treviso TV, Italy)","name":"Palazzo Bianco Con Affresco - Tv","address":"Via Sant'Agostino, 27, 31100 Treviso TV, Italy","latE6":45666891,"lngE6":12249456,"team":"RESISTANCE"}]]
    DeployedFireworks {
        player: Player<'a>,
        portal: Portal<'a>,
    },
    MaybeVirus {
        player: Player<'a>,
        portal: Portal<'a>,
    },
    Unknown(&'a str),
}

impl<'a> TryFrom<(PlextType, &'a ingress_intel_rs::plexts::Plext)> for Plext<'a> {
    type Error = ();
    fn try_from(
        (pt, plext): (PlextType, &'a ingress_intel_rs::plexts::Plext),
    ) -> Result<Self, Self::Error> {
        Ok(match pt {
            PlextType::Captured => Plext::Captured {
                player: plext
                    .markup
                    .get(0)
                    .ok_or_else(|| error!("Can't find player on markup 0: {:?}", plext))
                    .and_then(TryInto::try_into)?,
                portal: plext
                    .markup
                    .get(2)
                    .ok_or_else(|| error!("Can't find portal on markup 2: {:?}", plext))
                    .and_then(TryInto::try_into)?,
            },
            PlextType::CreatedCF => Plext::CreatedCF {
                player: plext
                    .markup
                    .get(0)
                    .ok_or_else(|| error!("Can't find player on markup 0: {:?}", plext))
                    .and_then(TryInto::try_into)?,
                portal: plext
                    .markup
                    .get(2)
                    .ok_or_else(|| error!("Can't find portal on markup 2: {:?}", plext))
                    .and_then(TryInto::try_into)?,
                mu: plext
                    .markup
                    .get(4)
                    .ok_or_else(|| error!("Can't find mu on markup 4: {:?}", plext))
                    .and_then(|(_, m)| {
                        m.plain
                            .parse()
                            .map_err(|e| error!("Invalid MU value: {}", e))
                    })?,
            },
            PlextType::DestroyedCF => Plext::DestroyedCF {
                player: plext
                    .markup
                    .get(0)
                    .ok_or_else(|| error!("Can't find player on markup 0: {:?}", plext))
                    .and_then(TryInto::try_into)?,
                portal: plext
                    .markup
                    .get(2)
                    .ok_or_else(|| error!("Can't find portal on markup 2: {:?}", plext))
                    .and_then(TryInto::try_into)?,
                mu: plext
                    .markup
                    .get(4)
                    .ok_or_else(|| error!("Can't find mu on markup 4: {:?}", plext))
                    .and_then(|(_, m)| {
                        m.plain
                            .parse()
                            .map_err(|e| error!("Invalid MU value: {}", e))
                    })?,
            },
            PlextType::DeployedReso => Plext::DeployedReso {
                player: plext
                    .markup
                    .get(0)
                    .ok_or_else(|| error!("Can't find player on markup 0: {:?}", plext))
                    .and_then(TryInto::try_into)?,
                portal: plext
                    .markup
                    .get(2)
                    .ok_or_else(|| error!("Can't find portal on markup 2: {:?}", plext))
                    .and_then(TryInto::try_into)?,
            },
            PlextType::DestroyedReso => Plext::DestroyedReso {
                player: plext
                    .markup
                    .get(0)
                    .ok_or_else(|| error!("Can't find player on markup 0: {:?}", plext))
                    .and_then(TryInto::try_into)?,
                portal: plext
                    .markup
                    .get(2)
                    .ok_or_else(|| error!("Can't find portal on markup 2: {:?}", plext))
                    .and_then(TryInto::try_into)?,
            },
            PlextType::DestroyedLink => Plext::DestroyedLink {
                player: plext
                    .markup
                    .get(0)
                    .ok_or_else(|| error!("Can't find player on markup 0: {:?}", plext))
                    .and_then(TryInto::try_into)?,
                source: plext
                    .markup
                    .get(2)
                    .ok_or_else(|| error!("Can't find source on markup 2: {:?}", plext))
                    .and_then(TryInto::try_into)?,
                target: plext
                    .markup
                    .get(4)
                    .ok_or_else(|| error!("Can't find target on markup 4: {:?}", plext))
                    .and_then(TryInto::try_into)?,
            },
            PlextType::Linked => Plext::Linked {
                player: plext
                    .markup
                    .get(0)
                    .ok_or_else(|| error!("Can't find player on markup 0: {:?}", plext))
                    .and_then(TryInto::try_into)?,
                source: plext
                    .markup
                    .get(2)
                    .ok_or_else(|| error!("Can't find source on markup 2: {:?}", plext))
                    .and_then(TryInto::try_into)?,
                target: plext
                    .markup
                    .get(4)
                    .ok_or_else(|| error!("Can't find target on markup 4: {:?}", plext))
                    .and_then(TryInto::try_into)?,
            },
            PlextType::DroneReturn => Plext::DroneReturn {
                player: plext
                    .markup
                    .get(1)
                    .ok_or_else(|| error!("Can't find player on markup 0: {:?}", plext))
                    .and_then(TryInto::try_into)?,
            },
            PlextType::DeployedBeacon => Plext::DeployedBeacon {
                player: plext
                    .markup
                    .get(0)
                    .ok_or_else(|| error!("Can't find player on markup 0: {:?}", plext))
                    .and_then(TryInto::try_into)?,
                portal: plext
                    .markup
                    .get(2)
                    .ok_or_else(|| error!("Can't find portal on markup 2: {:?}", plext))
                    .and_then(TryInto::try_into)?,
            },
            PlextType::DeployedFireworks => Plext::DeployedFireworks {
                player: plext
                    .markup
                    .get(0)
                    .ok_or_else(|| error!("Can't find player on markup 0: {:?}", plext))
                    .and_then(TryInto::try_into)?,
                portal: plext
                    .markup
                    .get(2)
                    .ok_or_else(|| error!("Can't find portal on markup 2: {:?}", plext))
                    .and_then(TryInto::try_into)?,
            },
            PlextType::Unknown => Plext::Unknown(plext.text.as_str()),
        })
    }
}

impl<'a> std::fmt::Display for Plext<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Plext::Captured { player, portal } => write!(
                f,
                "{} {}captured {}",
                player,
                unsafe { String::from_utf8_unchecked(vec![0xE2, 0x9B, 0xB3]) },
                portal
            ), //flag
            Plext::CreatedCF { player, portal, mu } => write!(
                f,
                "{} {}created a Control Field {} +{}MU",
                player,
                unsafe { String::from_utf8_unchecked(vec![0xE2, 0x9B, 0x9B]) },
                portal,
                mu
            ), //triangle
            Plext::DestroyedCF { player, portal, mu } => write!(
                f,
                "{} {}destroyed a Control Field {} -{}MU",
                player,
                unsafe { String::from_utf8_unchecked(vec![0xE2, 0xAD, 0x99]) },
                portal,
                mu
            ), //cross
            Plext::DeployedReso { player, portal } => write!(
                f,
                "{} {}deployed a Resonator on {}",
                player,
                unsafe { String::from_utf8_unchecked(vec![0xF0, 0x9F, 0xA7, 0xB1]) },
                portal
            ), //bricks
            Plext::DestroyedReso { player, portal } => write!(
                f,
                "{} {}destroyed a Resonator on {}",
                player,
                unsafe { String::from_utf8_unchecked(vec![0xF0, 0x9F, 0x92, 0xA5]) },
                portal
            ), //explosion
            Plext::DestroyedLink {
                player,
                source,
                target,
            } => write!(
                f,
                "{} {}destroyed the Link {} to {}",
                player,
                unsafe { String::from_utf8_unchecked(vec![0xE2, 0x9C, 0x82]) },
                source,
                target
            ), //scissors
            Plext::Linked {
                player,
                source,
                target,
            } => write!(
                f,
                "{} {}linked {} to {}",
                player,
                unsafe { String::from_utf8_unchecked(vec![0xF0, 0x9F, 0x94, 0x97]) },
                source,
                target
            ), //chain
            Plext::DroneReturn { player } => write!(
                f,
                "{}Drone returned to Agent by {}",
                unsafe { String::from_utf8_unchecked(vec![0xF0, 0x9F, 0x9B, 0xB8]) },
                player
            ), //ufo
            Plext::DeployedBeacon { player, portal } => write!(
                f,
                "{} {}deployed a Beacon on {}",
                player,
                unsafe { String::from_utf8_unchecked(vec![0xF0, 0x9F, 0x9A, 0xA8]) },
                portal
            ), //police
            Plext::DeployedFireworks { player, portal } => write!(
                f,
                "{} {}deployed Fireworks on {}",
                player,
                unsafe { String::from_utf8_unchecked(vec![0xF0, 0x9F, 0x8E, 0x86]) },
                portal
            ), //fireworks
            Plext::MaybeVirus { player, portal } => write!(
                f,
                "{} {}probably used a Virus on {}",
                player,
                unsafe { String::from_utf8_unchecked(vec![0xF0, 0x9F, 0xA6, 0xA0]) },
                portal
            ), //virus
            Plext::Unknown(s) => write!(f, "{}", s),
        }
    }
}

impl<'a> Plext<'a> {
    pub fn has_duplicates(&self, others: &[Plext<'a>]) -> bool {
        match self {
            Plext::DeployedReso { player, portal } => others.iter().any(|m| {
                m == &Plext::Captured {
                    player: *player,
                    portal: *portal,
                }
            }),
            _ => false,
        }
    }
}

impl<'a> DedupFlatten for Plext<'a> {
    fn dedup_flatten(&mut self) {
        if let Plext::DestroyedReso { player, portal } = *self {
            *self = Plext::MaybeVirus { player, portal };
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum Team {
    Enlightened,
    Resistance,
}

impl<'a> TryFrom<Option<&'a str>> for Team {
    type Error = ();
    fn try_from(s: Option<&'a str>) -> Result<Self, Self::Error> {
        match s {
            Some("ENLIGHTENED") => Ok(Team::Enlightened),
            Some("RESISTANCE") => Ok(Team::Resistance),
            _ => {
                error!("Unrecognized team {:?}", s);
                Err(())
            }
        }
    }
}

impl std::fmt::Display for Team {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Team::Enlightened => write!(f, "{}", unsafe {
                String::from_utf8_unchecked(vec![0xF0, 0x9F, 0x9F, 0xA2])
            }), // green circle
            Team::Resistance => write!(f, "{}", unsafe {
                String::from_utf8_unchecked(vec![0xF0, 0x9F, 0x94, 0xB5])
            }), // blue circle
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Player<'a> {
    team: Team,
    name: &'a str,
}

impl<'a> Player<'a> {
    pub fn get_name(&self) -> &'a str {
        self.name
    }
}

impl<'a> TryFrom<&'a Markup> for Player<'a> {
    type Error = ();
    fn try_from((mt, me): &'a Markup) -> Result<Self, Self::Error> {
        if mt == "PLAYER" {
            Ok(Player {
                team: me.team.as_deref().try_into()?,
                name: me.plain.as_str(),
            })
        } else {
            error!("Expected a PLAYER element, got {}", mt);
            Err(())
        }
    }
}

impl<'a> std::fmt::Display for Player<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}{}", self.team, self.name)
    }
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Portal<'a> {
    name: &'a str,
    address: &'a str,
    lat: Decimal,
    lon: Decimal,
}

impl<'a> Portal<'a> {
    pub fn get_coords(&self) -> (Decimal, Decimal) {
        (self.lat, self.lon)
    }
}

impl<'a> std::fmt::Display for Portal<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<a href=\"https://intel.ingress.com/intel?pll={},{}\">{}</a> (<a href=\"https://maps.google.it/maps/?q={},{}\">{}</a>)", self.lat, self.lon, self.name, self.lat, self.lon, self.address)
    }
}

impl<'a> TryFrom<&'a Markup> for Portal<'a> {
    type Error = ();
    fn try_from((mt, me): &'a Markup) -> Result<Self, Self::Error> {
        if mt == "PORTAL" {
            Ok(Portal {
                name: me.name.as_deref().ok_or_else(|| error!("Missing name"))?,
                address: me
                    .address
                    .as_deref()
                    .ok_or_else(|| error!("Missing address"))?,
                lat: me
                    .lat_e6
                    .map(|i| Decimal::new(i, 6))
                    .ok_or_else(|| error!("Missing latE6"))?,
                lon: me
                    .lng_e6
                    .map(|i| Decimal::new(i, 6))
                    .ok_or_else(|| error!("Missing lngE6"))?,
            })
        } else {
            error!("Expected a PORTAL element, got {}", mt);
            Err(())
        }
    }
}
