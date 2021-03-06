use std::{
  collections::{/*hash_map::Iter as HashIter,*/ btree_map::Iter as BTreeIter, HashMap, BTreeMap},
  iter::Map,
  slice::Iter,
  time::Instant,
};

use itertools::Itertools;
use log::{error,warn,debug,trace,info};

use crate::{
  dds::qos::HasQoSPolicy, network::util::get_local_multicast_locators, 
  structure::guid::GuidPrefix,
};

use crate::structure::{guid::GUID, guid::EntityId,
  duration::Duration, entity::RTPSEntity, locator::LocatorList};

use crate::{
  dds::{
    rtps_reader_proxy::RtpsReaderProxy, reader::Reader, participant::DomainParticipant,
    topic::Topic, rtps_writer_proxy::RtpsWriterProxy, traits::TopicDescription,
  },
};

use super::{
  discovery::Discovery,
  data_types::{
    spdp_participant_data::SPDPDiscoveredParticipantData,
    topic_data::{
      DiscoveredReaderData, DiscoveredTopicData, DiscoveredWriterData, ParticipantMessageData,
      ReaderProxy, SubscriptionBuiltinTopicData, TopicBuiltinTopicData,
    },
  },
};

// If remote participant does not specifiy lease duration, how long silence
// until we pronounce it dead.
const DEFAULT_PARTICIPANT_LEASE_DURATION : Duration = Duration::from_secs(60);


pub(crate) struct DiscoveryDB {
  participant_proxies: BTreeMap<GUID, SPDPDiscoveredParticipantData>,
  participant_last_life_signs: HashMap<GUID, Instant>,
  // local writer proxies for topics (topic name acts as key)
  local_topic_writers: HashMap<GUID, DiscoveredWriterData>,
  // local reader proxies for topics (topic name acts as key)
  local_topic_readers: HashMap<GUID, DiscoveredReaderData>,

  external_topic_readers: Vec<DiscoveredReaderData>,
  external_topic_writers: Vec<DiscoveredWriterData>,

  topics: HashMap<String, DiscoveredTopicData>,

  readers_updated: bool,
  writers_updated: bool,
}

impl DiscoveryDB {
  pub fn new() -> DiscoveryDB {
    DiscoveryDB {
      participant_proxies: BTreeMap::new(),
      participant_last_life_signs: HashMap::new(),
      local_topic_writers: HashMap::new(),
      local_topic_readers: HashMap::new(),
      external_topic_readers: Vec::new(),
      external_topic_writers: Vec::new(),
      topics: HashMap::new(),
      readers_updated: false,
      writers_updated: false,
    }
  }

  pub fn update_participant(&mut self, data: &SPDPDiscoveredParticipantData) -> bool {
    match data.participant_guid {
      Some(guid) => {
        debug!("update_participant: {:?}",&data);
        // sanity check
        if guid.entityId != EntityId::ENTITYID_PARTICIPANT {
          error!("Discovered participant GUID entity_id is not for participant: {:?}",guid);
          // Maybe we should discard the participant here?
        }
        // actual work here:
        self.participant_proxies.insert(guid, data.clone());
        self.participant_last_life_signs.insert(guid, Instant::now() );
        true
      }
      _ => false,
    }
  }

  pub fn remove_participant(&mut self, guid: GUID) {
    info!("removing participant {:?}",guid);
    self.participant_proxies.remove(&guid);
    self.participant_last_life_signs.remove(&guid);

    self.remove_topic_reader_with_prefix(guid.guidPrefix);

    self.remove_topic_writer_with_prefix(guid.guidPrefix);
  }

  pub fn find_participant_proxy(&self, guid_prefix: GuidPrefix) 
    -> Option<&SPDPDiscoveredParticipantData> 
  {
    self.participant_proxies
      .get( &GUID::new_with_prefix_and_id(guid_prefix, EntityId::ENTITYID_PARTICIPANT))
  }

  fn remove_topic_reader_with_prefix(&mut self, guid_prefix: GuidPrefix) {
    self
      .external_topic_readers
      .retain(|d| match d.reader_proxy.remote_reader_guid {
        Some(g) => g.guidPrefix != guid_prefix,
        // removing non existent guids
        None => false,
      });
  }

  pub fn remove_topic_reader(&mut self, guid: GUID) {
    self
      .external_topic_readers
      .retain(|d| match d.reader_proxy.remote_reader_guid {
        Some(g) => g != guid,
        // removing non existent guids
        None => false,
      });
  }

  fn remove_topic_writer_with_prefix(&mut self, guid_prefix: GuidPrefix) {
    self
      .external_topic_writers
      .retain(|d| match d.writer_proxy.remote_writer_guid {
        Some(g) => g.guidPrefix != guid_prefix,
        // removing non existent guids
        None => false,
      });
  }

  pub fn remove_topic_writer(&mut self, guid: GUID) {
    self
      .external_topic_writers
      .retain(|d| match d.writer_proxy.remote_writer_guid {
        Some(g) => g != guid,
        // removing non existent guids
        None => false,
      });
  }

  pub fn participant_cleanup(&mut self) {
    let inow = Instant::now();

    let mut to_remove = Vec::new();

    // TODO: We are not cleaning up liast_life_signs table, but that should not be a problem,
    // except for a slight memory leak.
    for (&guid,sp) in self.participant_proxies.iter() {
      let lease_duration = sp.lease_duration
            .unwrap_or( DEFAULT_PARTICIPANT_LEASE_DURATION );

      match self.participant_last_life_signs.get(&guid) {
        Some(&last_life) =>
          // keep, if duration not exeeded
          if Duration::from_std(inow.duration_since(last_life)) <= lease_duration {
            () // this is a keeper
          } else {
            debug!("participant cleanup - deleting participant proxy {:?}",guid);
            to_remove.push(guid);    
          },
        None => {
          error!("Participant {:?} not in last_life_signs table?", guid);
        }
      } // match
    } // for
    for guid in to_remove {
      self.participant_proxies.remove(&guid).expect("Removing fail 1");
      self.participant_last_life_signs.remove(&guid).expect("Removing fail 2");
    }
  }

  fn topic_has_writers_or_readers(&self, topic_name: &String) -> bool {
    if let Some(_) =
      self
        .local_topic_readers
        .iter()
        .find(|(_, p)| match &p.subscription_topic_data.topic_name() {
          Some(tn) => tn == topic_name,
          None => false,
        })
    {
      return true;
    }

    if let Some(_) =
      self
        .local_topic_writers
        .iter()
        .find(|(_, p)| match &p.publication_topic_data.topic_name {
          Some(tn) => tn == topic_name,
          None => false,
        })
    {
      return true;
    }

    if let Some(_) =
      self
        .external_topic_readers
        .iter()
        .find(|p| match &p.subscription_topic_data.topic_name() {
          Some(tn) => tn == topic_name,
          None => false,
        })
    {
      return true;
    }

    if let Some(_) =
      self
        .external_topic_writers
        .iter()
        .find(|p| match &p.publication_topic_data.topic_name {
          Some(tn) => tn == topic_name,
          None => false,
        })
    {
      return true;
    }

    false
  }

  pub fn topic_cleanup(&mut self) {
    // removing topics that have no readers or writers
    let dead_topics: Vec<_> = self
      .topics
      .iter()
      .map(|(tn, _)| tn)
      .filter(|tn| !self.topic_has_writers_or_readers(tn))
      .map(|tn| tn.clone())
      .collect();
    for dt in dead_topics.iter() {
      self.topics.remove(dt);
    }
  }

  pub fn get_participants<'a>(
    &'a self,
  ) -> Map<
    BTreeIter<'a, GUID, SPDPDiscoveredParticipantData>,
    fn((&GUID, &'a SPDPDiscoveredParticipantData)) -> &'a SPDPDiscoveredParticipantData,
  > {
    type cvfun<'a> =
      fn((&GUID, &'a SPDPDiscoveredParticipantData)) -> &'a SPDPDiscoveredParticipantData;
    fn conver<'a>(
      (_, data): (&GUID, &'a SPDPDiscoveredParticipantData),
    ) -> &'a SPDPDiscoveredParticipantData {
      data
    }
    let cv: cvfun = conver;
    let a = self.participant_proxies.iter().map(cv);
    a
  }

  pub fn update_local_topic_writer(&mut self, writer: DiscoveredWriterData) {
    let writer_guid = match writer.writer_proxy.remote_writer_guid {
      Some(g) => g,
      None => return,
    };

    self.local_topic_writers.insert(writer_guid, writer);
    self.writers_updated = true;
  }

  pub fn remove_local_topic_writer(&mut self, guid: GUID) {
    self.local_topic_writers.remove(&guid);
    self.writers_updated = true;
  }

  pub fn get_external_reader_proxies<'a>(&'a self) -> Iter<'a, DiscoveredReaderData> {
    self.external_topic_readers.iter()
  }

  pub fn get_external_writer_proxies<'a>(&'a self) -> Iter<'a, DiscoveredWriterData> {
    self.external_topic_writers.iter()
  }

  fn add_reader_to_local_writer(&mut self, data: &DiscoveredReaderData) {
    let topic_name = match data.subscription_topic_data.topic_name().as_ref() {
      Some(tn) => tn,
      None => return,
    };
    // find out default LocatorsLists from Participant proxy
    let drd = data;
    let remote_reader_guid = 
      drd.reader_proxy.remote_reader_guid
        .expect("ReaderProxy has no GUID");
    let locator_lists = 
      self.find_participant_proxy(remote_reader_guid.guidPrefix)
        .map(|pp| {
          debug!("Added default locators to Reader {:?}", remote_reader_guid);
          ( pp.default_unicast_locators.clone(), 
            pp.default_multicast_locators.clone() ) } )
        .unwrap_or_else( || {
            warn!("No remote participant known for {:?}\nSearched with {:?} in {:?}"
              ,drd, remote_reader_guid.guidPrefix, self.participant_proxies.keys() );
            (LocatorList::new(), LocatorList::new()) 
          } );

    let reader_proxy = RtpsReaderProxy::from_discovered_reader_data(data, 
                          locator_lists.0 , locator_lists.1 );

    match reader_proxy {
      Some(rp) => self
        .local_topic_writers
        .iter_mut()
        .filter(
          |(_, p)| match p.publication_topic_data.topic_name.as_ref() {
            Some(tn) => *tn == *topic_name,
            None => false,
          },
        )
        .for_each(|(_, p)| {
          p.writer_proxy
            .unicast_locator_list
            .append(&mut rp.unicast_locator_list.clone());
          p.writer_proxy.unicast_locator_list = p
            .writer_proxy
            .unicast_locator_list
            .clone()
            .into_iter()
            .unique()
            .collect();

          // TODO: multicast locators
        }),
      None => return,
    }
  }

  fn add_writer_to_local_reader(&mut self, data: &DiscoveredWriterData) {
    let topic_name = match data.publication_topic_data.topic_name.as_ref() {
      Some(tn) => tn,
      None => return,
    };

    let writer_proxy = RtpsWriterProxy::from_discovered_writer_data(data);

    match writer_proxy {
      Some(wp) => self
        .local_topic_readers
        .iter_mut()
        .filter(
          |(_, p)| match p.subscription_topic_data.topic_name().as_ref() {
            Some(tn) => *tn == *topic_name,
            None => false,
          },
        )
        .for_each(|(_, p)| {
          p.reader_proxy
            .unicast_locator_list
            .append(&mut wp.unicast_locator_list.clone());
          p.reader_proxy.unicast_locator_list = p
            .reader_proxy
            .unicast_locator_list
            .clone()
            .into_iter()
            .unique()
            .collect();

          // TODO: multicast locators
        }),
      None => return,
    }
  }

  pub fn update_subscription(&mut self, data: &DiscoveredReaderData) {
    self.add_reader_to_local_writer(data);
    debug!("External reader: {:?}",data);
    self.external_topic_readers.push(data.clone());
    self.external_topic_readers = self
      .external_topic_readers
      .clone()
      .into_iter()
      .unique()
      .collect();
  }

  pub fn update_publication(&mut self, data: &DiscoveredWriterData) {
    self.add_writer_to_local_reader(data);
    debug!("External writer: {:?}",data);
    self.external_topic_writers.push(data.clone());
    self.external_topic_writers = self
      .external_topic_writers
      .clone()
      .into_iter()
      .unique()
      .collect();
  }

  pub fn update_topic_data_drd(&mut self, drd: &DiscoveredReaderData) {
    let topic_data = DiscoveredTopicData::new(TopicBuiltinTopicData {
      key: None,
      name: drd.subscription_topic_data.topic_name().clone(),
      type_name: drd.subscription_topic_data.type_name().clone(),
      durability: drd.subscription_topic_data.durability().clone(),
      deadline: drd.subscription_topic_data.deadline().clone(),
      latency_budget: drd.subscription_topic_data.latency_budget().clone(),
      liveliness: drd.subscription_topic_data.liveliness().clone(),
      reliability: drd.subscription_topic_data.reliability().clone(),
      lifespan: drd.subscription_topic_data.lifespan().clone(),
      destination_order: drd.subscription_topic_data.destination_order().clone(),
      presentation: drd.subscription_topic_data.presentation().clone(),
      history: None,
      resource_limits: None,
      ownership: drd.subscription_topic_data.ownership().clone(),
    });

    self.update_topic_data(&topic_data);
  }

  pub fn update_topic_data_dwd(&mut self, dwd: &DiscoveredWriterData) {
    let topic_data = DiscoveredTopicData::new(TopicBuiltinTopicData {
      key: None,
      name: dwd.publication_topic_data.topic_name.clone(),
      type_name: dwd.publication_topic_data.type_name.clone(),
      durability: dwd.publication_topic_data.durability.clone(),
      deadline: dwd.publication_topic_data.deadline.clone(),
      latency_budget: dwd.publication_topic_data.latency_budget.clone(),
      liveliness: dwd.publication_topic_data.liveliness.clone(),
      reliability: dwd.publication_topic_data.reliability.clone(),
      lifespan: dwd.publication_topic_data.lifespan.clone(),
      destination_order: dwd.publication_topic_data.destination_order.clone(),
      presentation: dwd.publication_topic_data.presentation.clone(),
      history: None,
      resource_limits: None,
      ownership: dwd.publication_topic_data.ownership.clone(),
    });

    self.update_topic_data(&topic_data);
  }

  pub fn update_topic_data_p(&mut self, topic: &Topic) {
    let topic_data = DiscoveredTopicData::new(TopicBuiltinTopicData {
      key: None,
      name: Some(String::from(topic.get_name())),
      type_name: Some(String::from(topic.get_type().name())),
      durability: topic.get_qos().durability.clone(),
      deadline: topic.get_qos().deadline.clone(),
      latency_budget: topic.get_qos().latency_budget.clone(),
      liveliness: topic.get_qos().liveliness.clone(),
      reliability: topic.get_qos().reliability.clone(),
      lifespan: topic.get_qos().lifespan.clone(),
      destination_order: topic.get_qos().destination_order.clone(),
      presentation: topic.get_qos().presentation.clone(),
      history: topic.get_qos().history.clone(),
      resource_limits: topic.get_qos().resource_limits.clone(),
      ownership: topic.get_qos().ownership.clone(),
    });

    self.update_topic_data(&topic_data);
  }

  pub fn update_topic_data(&mut self, data: &DiscoveredTopicData) -> bool {
    trace!("Update topic data: {:?}",&data);
    let topic_name = match &data.topic_data.name {
      Some(n) => n,
      None => {
        warn!("Received DiscoveredTopicData doesn't have a name.");
        return false;
      }
    };

    match self.topics.get_mut(topic_name) {
      Some(t) => *t = data.clone(),
      None => {
        self.topics.insert(topic_name.clone(), data.clone());
      }
    };

    true
  }

  pub fn initialize_participant_reader_proxy(&mut self, port: u16) {
    let guid = GUID::new_with_prefix_and_id(
      GuidPrefix::GUIDPREFIX_UNKNOWN,
      EntityId::ENTITYID_SPDP_BUILTIN_PARTICIPANT_READER,
    );
    let mut reader_proxy = ReaderProxy::new(guid);
    reader_proxy.multicast_locator_list = get_local_multicast_locators(port);

    let sub_topic_data = SubscriptionBuiltinTopicData::new(
      guid,
      &String::from("DCPSParticipant"),
      &String::from("SPDPDiscoveredParticipantData"),
      &Discovery::PARTICIPANT_MESSAGE_QOS,
    );
    let drd = DiscoveredReaderData {
      reader_proxy,
      subscription_topic_data: sub_topic_data,
      content_filter: None,
    };

    self.add_reader_to_local_writer(&drd);
  }

  // local topic readers
  pub fn update_local_topic_reader(
    &mut self,
    domain_participant: &DomainParticipant,
    topic: &Topic,
    reader: &Reader,
  ) {
    let reader_guid = reader.get_guid();

    let reader_proxy = RtpsReaderProxy::from_reader(
      reader,
      domain_participant.domain_id(),
      domain_participant.participant_id(),
    );

    let mut subscription_data = SubscriptionBuiltinTopicData::new(
      reader_guid,
      &topic.get_name(),
      &topic.get_type().name(),
      &topic.get_qos(),
    );
    subscription_data.set_participant_key(domain_participant.get_guid());

    // TODO: possibly change content filter to dynamic value
    let content_filter = None;

    let discovered_reader_data = DiscoveredReaderData {
      reader_proxy: ReaderProxy::from(reader_proxy),
      subscription_topic_data: subscription_data,
      content_filter,
    };

    self
      .local_topic_readers
      .insert(reader_guid, discovered_reader_data);

    self.readers_updated = true;
  }

  pub fn remove_local_topic_reader(&mut self, guid: GUID) {
    self.local_topic_readers.remove(&guid);
    self.readers_updated = true;
  }

  pub fn is_readers_updated(&self) -> bool {
    self.readers_updated
  }

  pub fn readers_updated(&mut self, updated: bool) {
    self.readers_updated = updated;
  }

  pub fn is_writers_updated(&self) -> bool {
    self.writers_updated
  }

  pub fn writers_updated(&mut self, updated: bool) {
    self.writers_updated = updated;
  }

  pub fn get_all_local_topic_readers<'a>(
    &'a self,
  ) -> impl Iterator<Item = &'a DiscoveredReaderData> {
    self.local_topic_readers.iter().map(|(_, p)| p)
  }

  pub fn get_all_local_topic_writers<'a>(
    &'a self,
  ) -> impl Iterator<Item = &'a DiscoveredWriterData> {
    self.local_topic_writers.iter().map(|(_, p)| p)
  }

  pub fn get_all_topics<'a>(&'a self) -> impl Iterator<Item = &'a DiscoveredTopicData> {
    self
      .topics
      .iter()
      .filter(|(s, _)| !s.starts_with("DCPS"))
      .map(|(_, v)| v)
  }

  // TODO: return iterator somehow?
  pub fn get_local_topic_readers<'a, T: TopicDescription>(
    &'a self,
    topic: &'a T,
  ) -> Vec<&DiscoveredReaderData> {
    let topic_name = String::from(topic.get_name());
    self
      .local_topic_readers
      .iter()
      .filter(|(_, p)| {
        *match p.subscription_topic_data.topic_name().as_ref() {
          Some(t) => t,
          None => return false,
        } == topic_name
      })
      .map(|(_, p)| p)
      .collect()
  }

  pub fn update_lease_duration(&mut self, data: ParticipantMessageData) {
    let i = Instant::now();
    self
      .external_topic_writers
      .iter_mut()
      .filter(|p| match p.writer_proxy.remote_writer_guid {
        Some(g) => g.guidPrefix == data.guid,
        None => false,
      })
      .for_each(|p| p.last_updated = i);
  }
}

#[cfg(test)]

mod tests {
  use super::*;

  use crate::{
    dds::qos::QosPolicies,
    structure::dds_cache::DDSCache,
    dds::topic::TopicKind,
    test::{
      random_data::RandomData,
      test_data::{subscription_builtin_topic_data, spdp_participant_data, reader_proxy_data},
    },
  };
  use std::sync::{RwLock, Arc};

  use crate::structure::guid::*;
  use crate::serialization::cdr_serializer::CDRSerializerAdapter;
  use byteorder::LittleEndian;
  use std::time::Duration as StdDuration;
  use crate::dds::values::result::StatusChange;
  use crate::dds::with_key::datareader::ReaderCommand;

  #[test]
  fn discdb_participant_operations() {
    let mut discoverydb = DiscoveryDB::new();
    let mut data = spdp_participant_data().unwrap();
    data.lease_duration = Some(Duration::from(StdDuration::from_secs(1)));

    discoverydb.update_participant(&data);
    assert!(discoverydb.participant_proxies.len() == 1);

    discoverydb.update_participant(&data);
    assert!(discoverydb.participant_proxies.len() == 1);

    std::thread::sleep(StdDuration::from_secs(2));
    discoverydb.participant_cleanup();
    assert!(discoverydb.participant_proxies.len() == 0);

    // TODO: more operations tests
  }

  #[test]
  fn discdb_writer_proxies() {
    let _discoverydb = DiscoveryDB::new();
    let topic_name = String::from("some_topic");
    let type_name = String::from("RandomData");
    let _dreader = DiscoveredReaderData::default(&topic_name, &type_name);

    // TODO: more tests :)
  }

  #[test]
  fn discdb_subscription_operations() {
    let mut discovery_db = DiscoveryDB::new();

    let domain_participant = DomainParticipant::new(0);
    let topic = domain_participant
      .create_topic(
        "Foobar",
        "RandomData",
        &QosPolicies::qos_none(),
        TopicKind::WithKey,
      )
      .unwrap();
    let topic2 = domain_participant
      .create_topic(
        "Barfoo",
        "RandomData",
        &QosPolicies::qos_none(),
        TopicKind::WithKey,
      )
      .unwrap();

    let publisher1 = domain_participant
      .create_publisher(&QosPolicies::qos_none())
      .unwrap();
    let dw = publisher1
      .create_datawriter::<RandomData, CDRSerializerAdapter<RandomData, LittleEndian>>(
        None, topic.clone(), None,
      )
      .unwrap();

    let writer_data = DiscoveredWriterData::new(&dw, &topic, &domain_participant);

    let _writer_key = writer_data.writer_proxy.remote_writer_guid.unwrap().clone();
    discovery_db.update_local_topic_writer(writer_data);
    assert_eq!(discovery_db.local_topic_writers.len(), 1);

    let publisher2 = domain_participant
      .create_publisher(&QosPolicies::qos_none())
      .unwrap();
    let dw2 = publisher2
      .create_datawriter::<RandomData, CDRSerializerAdapter<RandomData, LittleEndian>>(
        None, topic.clone(), None,
      )
      .unwrap();
    let writer_data2 = DiscoveredWriterData::new(&dw2, &topic, &domain_participant);
    let _writer2_key = writer_data2
      .writer_proxy
      .remote_writer_guid
      .unwrap()
      .clone();
    discovery_db.update_local_topic_writer(writer_data2);
    assert_eq!(discovery_db.local_topic_writers.len(), 2);

    // creating data
    let reader1 = reader_proxy_data().unwrap();
    let mut reader1sub = subscription_builtin_topic_data().unwrap();
    reader1sub.set_key(reader1.remote_reader_guid.unwrap());
    reader1sub.set_topic_name(&topic.get_name());
    let dreader1 = DiscoveredReaderData {
      reader_proxy: reader1.clone(),
      subscription_topic_data: reader1sub.clone(),
      content_filter: None,
    };
    discovery_db.update_subscription(&dreader1);

    let reader2 = reader_proxy_data().unwrap();
    let mut reader2sub = subscription_builtin_topic_data().unwrap();
    reader2sub.set_key(reader2.remote_reader_guid.unwrap());
    reader2sub.set_topic_name(&topic2.get_name());
    let dreader2 = DiscoveredReaderData {
      reader_proxy: reader2,
      subscription_topic_data: reader2sub,
      content_filter: None,
    };
    discovery_db.update_subscription(&dreader2);

    let reader3 = reader1.clone();
    let reader3sub = reader1sub.clone();
    let dreader3 = DiscoveredReaderData {
      reader_proxy: reader3,
      subscription_topic_data: reader3sub,
      content_filter: None,
    };
    discovery_db.update_subscription(&dreader3);

    // TODO: there might be a need for different scenarios
  }

  #[test]
  fn discdb_local_topic_reader() {
    let dp = DomainParticipant::new(0);
    let topic = dp
      .create_topic(
        "some topic name",
        "Wazzup",
        &QosPolicies::qos_none(),
        TopicKind::WithKey,
      )
      .unwrap();
    let mut discoverydb = DiscoveryDB::new();

    let (notification_sender, _notification_receiver) = mio_extras::channel::sync_channel(100);
    let (status_sender, _status_reciever) = mio_extras::channel::sync_channel::<StatusChange>(100);
    let (_reader_commander1, reader_command_receiver1) =
      mio_extras::channel::sync_channel::<ReaderCommand>(100);
    let (_reader_commander2, reader_command_receiver2) =
      mio_extras::channel::sync_channel::<ReaderCommand>(100);

    let reader = Reader::new(
      GUID::dummy_test_guid(EntityKind::READER_NO_KEY_USER_DEFINED),
      notification_sender.clone(),
      status_sender.clone(),
      Arc::new(RwLock::new(DDSCache::new())),
      topic.get_name().to_string(),
      reader_command_receiver1,
    );

    discoverydb.update_local_topic_reader(&dp, &topic, &reader);
    assert_eq!(discoverydb.local_topic_readers.len(), 1);
    assert_eq!(discoverydb.get_local_topic_readers(&topic).len(), 1);

    discoverydb.update_local_topic_reader(&dp, &topic, &reader);
    assert_eq!(discoverydb.local_topic_readers.len(), 1);
    assert_eq!(discoverydb.get_local_topic_readers(&topic).len(), 1);

    let reader = Reader::new(
      GUID::dummy_test_guid(EntityKind::READER_NO_KEY_USER_DEFINED),
      notification_sender.clone(),
      status_sender.clone(),
      Arc::new(RwLock::new(DDSCache::new())),
      topic.get_name().to_string(),
      reader_command_receiver2,
    );

    discoverydb.update_local_topic_reader(&dp, &topic, &reader);
    assert_eq!(discoverydb.get_local_topic_readers(&topic).len(), 2);
    assert_eq!(discoverydb.get_all_local_topic_readers().count(), 2);
  }
}
