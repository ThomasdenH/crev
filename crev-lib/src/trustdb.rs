use crate::VerificationStatus;
use chrono::{self, offset::Utc, DateTime};
use crev_data::{
    self,
    proof::review::Rating,
    proof::trust::TrustLevel,
    proof::{self, review, Content, ContentCommon},
    Digest, Id, Url,
};
use default::default;
use std::collections::BTreeMap;
use std::collections::{hash_map, BTreeSet, HashMap, HashSet};

pub struct Timestamped<T> {
    pub date: chrono::DateTime<Utc>,
    value: T,
}

impl<T> Timestamped<T> {
    fn update_to_more_recent(&mut self, date: &chrono::DateTime<Utc>, value: T) {
        if self.date < *date {
            self.value = value;
        }
    }

    fn insert_into_or_update_to_more_recent<K>(self, entry: hash_map::Entry<K, Timestamped<T>>) {
        match entry {
            hash_map::Entry::Occupied(mut entry) => entry
                .get_mut()
                .update_to_more_recent(&self.date, self.value),
            hash_map::Entry::Vacant(entry) => {
                entry.insert(self);
            }
        }
    }
}

type TimestampedUrl = Timestamped<Url>;
type TimestampedTrustLevel = Timestamped<TrustLevel>;
type TimestampedReview = Timestamped<review::Review>;

impl From<proof::Trust> for TimestampedTrustLevel {
    fn from(trust: proof::Trust) -> Self {
        TimestampedTrustLevel {
            date: trust.date().with_timezone(&Utc),
            value: trust.trust,
        }
    }
}

impl<'a, T: review::Common> From<&'a T> for TimestampedReview {
    fn from(review: &T) -> Self {
        TimestampedReview {
            value: review.review().to_owned(),
            date: review.date().with_timezone(&Utc),
        }
    }
}

/// In memory database tracking information from proofs
///
/// After population, used for calculating the effcttive trust set, etc.
pub struct TrustDB {
    trust_id_to_id: HashMap<Id, HashMap<Id, TimestampedTrustLevel>>, // who -(trusts)-> whom
    digest_to_reviews: HashMap<Vec<u8>, HashMap<Id, TimestampedReview>>, // what (digest) -(reviewed)-> by whom
    url_by_id: HashMap<Id, TimestampedUrl>,
    url_by_id_secondary: HashMap<Id, TimestampedUrl>,

    package_review_by_signature: HashMap<String, review::Package>,
    package_reviews_by_source: BTreeMap<String, BTreeSet<String>>,
    package_reviews_by_name: BTreeMap<(String, String), BTreeSet<String>>,
    package_reviews_by_version: BTreeMap<(String, String, String), BTreeSet<String>>,
}

impl Default for TrustDB {
    fn default() -> Self {
        Self {
            trust_id_to_id: Default::default(),
            url_by_id: Default::default(),
            url_by_id_secondary: Default::default(),
            digest_to_reviews: Default::default(),
            package_review_by_signature: default(),
            package_reviews_by_source: default(),
            package_reviews_by_name: default(),
            package_reviews_by_version: default(),
        }
    }
}

impl TrustDB {
    pub fn new() -> Self {
        default()
    }

    fn add_code_review(&mut self, review: &review::Code) {
        let from = &review.from;
        self.record_url_from_from_field(&review.date_utc(), &from);
        for file in &review.files {
            TimestampedReview::from(review).insert_into_or_update_to_more_recent(
                self.digest_to_reviews
                    .entry(file.digest.to_owned())
                    .or_insert_with(HashMap::new)
                    .entry(from.id.clone()),
            )
        }
    }

    fn add_package_review(&mut self, review: &review::Package, signature: &str) {
        let from = &review.from;
        self.record_url_from_from_field(&review.date_utc(), &from);

        TimestampedReview::from(review).insert_into_or_update_to_more_recent(
            self.digest_to_reviews
                .entry(review.package.digest.to_owned())
                .or_insert_with(HashMap::new)
                .entry(from.id.clone()),
        );

        self.package_review_by_signature
            .entry(signature.to_owned())
            .or_insert_with(|| review.to_owned());

        self.package_reviews_by_source
            .entry(review.package.source.to_owned())
            .or_default()
            .insert(signature.to_owned());
        self.package_reviews_by_name
            .entry((
                review.package.source.to_owned(),
                review.package.name.to_owned(),
            ))
            .or_default()
            .insert(signature.to_owned());
        self.package_reviews_by_version
            .entry((
                review.package.source.to_owned(),
                review.package.name.to_owned(),
                review.package.version.to_owned(),
            ))
            .or_default()
            .insert(signature.to_owned());
    }

    pub fn get_package_review_count(
        &self,
        source: &str,
        name: Option<&str>,
        version: Option<&str>,
    ) -> usize {
        match (name, version) {
            (Some(name), Some(version)) => self
                .package_reviews_by_version
                .get(&(source.to_owned(), name.to_owned(), version.to_owned()))
                .map(|set| set.len())
                .unwrap_or(0),
            (Some(name), None) => self
                .package_reviews_by_name
                .get(&(source.to_owned(), name.to_owned()))
                .map(|set| set.len())
                .unwrap_or(0),
            (None, None) => self
                .package_reviews_by_source
                .get(source)
                .map(|set| set.len())
                .unwrap_or(0),
            (None, Some(_)) => panic!("Wrong usage"),
        }
    }
    
    pub fn get_package_reviews_for_package(
        &self,
        source: &str,
        name: Option<&str>,
        version: Option<&str>,
    ) -> impl Iterator<Item = proof::review::Package> {
        let mut proofs: Vec<_> = match (name, version) {
            (Some(name), Some(version)) => self
                .package_reviews_by_version
                .get(&(source.to_owned(), name.to_owned(), version.to_owned()))
                .map(|set| {
                    set.iter()
                        .map(|signature| self.package_review_by_signature[signature].clone())
                        .collect()
                })
                .unwrap_or_else(|| vec![]),

            (Some(name), None) => self
                .package_reviews_by_name
                .get(&(source.to_owned(), name.to_owned()))
                .map(|set| {
                    set.iter()
                        .map(|signature| self.package_review_by_signature[signature].clone())
                        .collect()
                })
                .unwrap_or_else(|| vec![]),
            (None, None) => self
                .package_reviews_by_source
                .get(source)
                .map(|set| {
                    set.iter()
                        .map(|signature| self.package_review_by_signature[signature].clone())
                        .collect()
                })
                .unwrap_or_else(|| vec![]),
            (None, Some(_)) => panic!("Wrong usage"),
        };

        proofs.sort_by(|a, b| a.date().cmp(&b.date()));

        proofs.into_iter()
    }

    fn add_trust_raw(&mut self, from: &Id, to: &Id, date: DateTime<Utc>, trust: TrustLevel) {
        TimestampedTrustLevel { value: trust, date }.insert_into_or_update_to_more_recent(
            self.trust_id_to_id
                .entry(from.to_owned())
                .or_insert_with(HashMap::new)
                .entry(to.to_owned()),
        );
    }

    fn add_trust(&mut self, trust: &proof::Trust) {
        let from = &trust.from;
        self.record_url_from_from_field(&trust.date_utc(), &from);
        for to in &trust.ids {
            self.add_trust_raw(&from.id, &to.id, trust.date_utc(), trust.trust);
        }
        for to in &trust.ids {
            self.record_url_from_to_field(&trust.date_utc(), &to)
        }
    }

    pub fn all_known_ids(&self) -> BTreeSet<Id> {
        self.url_by_id
            .keys()
            .chain(self.url_by_id_secondary.keys())
            .cloned()
            .collect()
    }

    fn get_reviews_of(&self, digest: &Digest) -> Option<&HashMap<Id, TimestampedReview>> {
        self.digest_to_reviews.get(digest.as_slice())
    }

    pub fn verify_digest<H>(
        &self,
        digest: &Digest,
        trust_set: &HashSet<Id, H>,
    ) -> VerificationStatus
    where
        H: std::hash::BuildHasher + std::default::Default,
    {
        if let Some(reviews) = self.get_reviews_of(digest) {
            // Faster somehow maybe?
            let reviews_by: HashSet<Id, H> = reviews.keys().map(|s| s.to_owned()).collect();
            let matching_reviewers = trust_set.intersection(&reviews_by);
            let mut trust_count = 0;
            let mut distrust_count = 0;
            for matching_reviewer in matching_reviewers {
                if Rating::Neutral <= reviews[matching_reviewer].value.rating {
                    trust_count += 1;
                }
                if reviews[matching_reviewer].value.rating < Rating::Neutral {
                    distrust_count += 1;
                }
            }

            if distrust_count > 0 {
                VerificationStatus::Flagged
            } else if trust_count > 0 {
                VerificationStatus::Verified
            } else {
                VerificationStatus::Unknown
            }
        } else {
            VerificationStatus::Unknown
        }
    }

    fn record_url_from_to_field(&mut self, date: &DateTime<Utc>, to: &crev_data::PubId) {
        self.url_by_id_secondary
            .entry(to.id.clone())
            .or_insert_with(|| TimestampedUrl {
                value: to.url.clone(),
                date: *date,
            });
    }

    fn record_url_from_from_field(&mut self, date: &DateTime<Utc>, from: &crev_data::PubId) {
        TimestampedUrl {
            value: from.url.clone(),
            date: date.to_owned(),
        }
        .insert_into_or_update_to_more_recent(self.url_by_id.entry(from.id.clone()));
    }
    fn add_proof(&mut self, proof: &proof::Proof) {
        proof
            .verify()
            .expect("All proofs were supposed to be valid here");
        match proof.content {
            Content::Code(ref review) => self.add_code_review(&review),
            Content::Package(ref review) => self.add_package_review(&review, &proof.signature),
            Content::Trust(ref trust) => self.add_trust(&trust),
        }
    }

    pub fn import_from_iter(&mut self, i: impl Iterator<Item = proof::Proof>) {
        for proof in i {
            self.add_proof(&proof);
        }
    }

    fn get_ids_trusted_by(&self, id: &Id) -> impl Iterator<Item = (TrustLevel, &Id)> {
        if let Some(map) = self.trust_id_to_id.get(id) {
            Some(map.iter().map(|(id, trust)| (trust.value, id)))
        } else {
            None
        }
        .into_iter()
        .flatten()
    }

    // Oh god, please someone verify this :D
    pub fn calculate_trust_set(&self, for_id: &Id, params: &TrustDistanceParams) -> HashSet<Id> {
        #[derive(PartialOrd, Ord, Eq, PartialEq, Clone, Debug)]
        struct Visit {
            distance: u64,
            id: Id,
        }
        let mut pending = BTreeSet::new();
        pending.insert(Visit {
            distance: 0,
            id: for_id.clone(),
        });

        let mut visited = HashMap::<&Id, _>::new();
        visited.insert(&for_id, 0);
        while let Some(current) = pending.iter().next().cloned() {
            pending.remove(&current);

            if let Some(visited_distance) = visited.get(&current.id) {
                if *visited_distance < current.distance {
                    continue;
                }
            }

            for (level, candidate_id) in self.get_ids_trusted_by(&&current.id) {
                let candidate_distance_from_current =
                    if let Some(v) = params.distance_by_level(level) {
                        v
                    } else {
                        continue;
                    };
                let candidate_total_distance = current.distance + candidate_distance_from_current;
                if candidate_total_distance > params.max_distance {
                    continue;
                }

                if let Some(prev_candidate_distance) = visited.get(candidate_id).cloned() {
                    if prev_candidate_distance > candidate_total_distance {
                        visited.insert(candidate_id, candidate_total_distance);
                        pending.insert(Visit {
                            distance: candidate_total_distance,
                            id: candidate_id.to_owned(),
                        });
                    }
                } else {
                    visited.insert(candidate_id, candidate_total_distance);
                    pending.insert(Visit {
                        distance: candidate_total_distance,
                        id: candidate_id.to_owned(),
                    });
                }
            }
        }

        visited.keys().map(|id| (*id).clone()).collect()
    }

    pub fn lookup_url(&self, id: &Id) -> Option<&Url> {
        self.url_by_id
            .get(id)
            .or_else(|| self.url_by_id_secondary.get(id))
            .map(|url| &url.value)
    }
}

pub struct TrustDistanceParams {
    pub max_distance: u64,
    pub high_trust_distance: u64,
    pub medium_trust_distance: u64,
    pub low_trust_distance: u64,
}

impl TrustDistanceParams {
    fn distance_by_level(&self, level: TrustLevel) -> Option<u64> {
        use crev_data::proof::trust::TrustLevel::*;
        Some(match level {
            Distrust => return Option::None,
            None => return Option::None,
            Low => self.low_trust_distance,
            Medium => self.medium_trust_distance,
            High => self.high_trust_distance,
        })
    }
}

impl Default for TrustDistanceParams {
    fn default() -> Self {
        Self {
            max_distance: 10,
            high_trust_distance: 0,
            medium_trust_distance: 1,
            low_trust_distance: 5,
        }
    }
}
