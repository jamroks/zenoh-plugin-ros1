//
// Copyright (c) 2022 ZettaScale Technology
//
// This program and the accompanying materials are made available under the
// terms of the Eclipse Public License 2.0 which is available at
// http://www.eclipse.org/legal/epl-2.0, or the Apache License, Version 2.0
// which is available at https://www.apache.org/licenses/LICENSE-2.0.
//
// SPDX-License-Identifier: EPL-2.0 OR Apache-2.0
//
// Contributors:
//   ZettaScale Zenoh Team, <zenoh@zettascale.tech>
//

use async_std::sync::Mutex;

use zenoh;

use std::sync::{
    atomic::{AtomicBool, Ordering::Relaxed},
    Arc,
};

use log::{debug, error};

use crate::ros_to_zenoh_bridge::{
    bridges_storage::BridgesStorage, discovery::LocalResources, environment::Environment,
    ros1_client, topic_mapping, zenoh_client,
};

use super::{
    discovery::{Discovery, DiscoveryBuilder},
    ros1_client::Ros1Client,
};

#[derive(PartialEq, Clone, Copy)]
pub enum RosStatus {
    Unknown,
    Synchronizing,
    Ok,
    Error,
}

#[derive(Default, PartialEq, Eq, Clone, Copy)]
pub struct BridgeStatus {
    pub ros_publishers: (usize, usize),
    pub ros_subscribers: (usize, usize),
    pub ros_services: (usize, usize),
    pub ros_clients: (usize, usize),
}

pub async fn work_cycle<RosStatusCallback, BridgeStatisticsCallback>(
    session: Arc<zenoh::Session>,
    flag: Arc<AtomicBool>,
    ros_status_callback: RosStatusCallback,
    statistics_callback: BridgeStatisticsCallback,
) where
    RosStatusCallback: Fn(RosStatus),
    BridgeStatisticsCallback: Fn(BridgeStatus),
{
    let ros1_client = Arc::new(ros1_client::Ros1Client::new(
        Environment::ros_name().get::<String>().as_str(),
    ));
    let zenoh_client = Arc::new(zenoh_client::ZenohClient::new(session.clone()));

    let local_resources = Arc::new(LocalResources::new(
        "*".to_string(),
        "*".to_string(),
        session.clone(),
    ));

    let bridges = Arc::new(Mutex::new(BridgesStorage::new(
        ros1_client.clone(),
        zenoh_client,
        local_resources,
    )));

    let _discovery = make_discovery(session.clone(), bridges.clone()).await;

    let mut bridge = RosToZenohBridge::new(ros_status_callback, statistics_callback);
    bridge.run(ros1_client, bridges, flag).await;
}

async fn make_discovery<'a>(
    session: Arc<zenoh::Session>,
    bridges: Arc<Mutex<BridgesStorage>>,
) -> Discovery {
    let bridges2 = bridges.clone();

    let builder = DiscoveryBuilder::new("*".to_string(), "*".to_string(), session);
    builder
        .on_discovered(move |b_type, topic| {
            let bridges = bridges.clone();
            Box::new(Box::pin(async move {
                bridges
                    .lock()
                    .await
                    .bridges()
                    .complementary_for(b_type)
                    .complementary_entity_discovered(topic)
                    .await;
            }))
        })
        .on_lost(move |b_type, topic| {
            let bridges = bridges2.clone();
            Box::new(Box::pin(async move {
                bridges
                    .lock()
                    .await
                    .bridges()
                    .complementary_for(b_type)
                    .complementary_entity_lost(topic)
                    .await;
            }))
        })
        .build()
        .await
}

struct RosToZenohBridge<RosStatusCallback, BridgeStatisticsCallback>
where
    RosStatusCallback: Fn(RosStatus),
    BridgeStatisticsCallback: Fn(BridgeStatus),
{
    ros_status: RosStatus,
    ros_status_callback: RosStatusCallback,

    statistics_callback: BridgeStatisticsCallback,
}

impl<RosStatusCallback, BridgeStatisticsCallback>
    RosToZenohBridge<RosStatusCallback, BridgeStatisticsCallback>
where
    RosStatusCallback: Fn(RosStatus),
    BridgeStatisticsCallback: Fn(BridgeStatus),
{
    // PUBLIC
    pub fn new(
        ros_status_callback: RosStatusCallback,
        statistics_callback: BridgeStatisticsCallback,
    ) -> Self {
        RosToZenohBridge {
            ros_status: RosStatus::Unknown,
            ros_status_callback,

            statistics_callback,
        }
    }

    pub async fn run(
        &mut self,
        ros1_client: Arc<Ros1Client>,
        bridges: Arc<Mutex<BridgesStorage>>,
        flag: Arc<AtomicBool>,
    ) {
        while flag.load(Relaxed) {
            let cl = ros1_client.clone();
            let ros1_state = async_std::task::spawn_blocking(move || {
                topic_mapping::Ros1TopicMapping::topic_mapping(cl.as_ref())
            })
            .await;

            debug!("ros state: {:#?}", ros1_state);

            match ros1_state {
                Ok(mut ros1_state_val) => {
                    self.transit_ros_status(RosStatus::Ok);

                    let smth_changed;
                    {
                        let mut locked = bridges.lock().await;
                        smth_changed = locked.receive_ros1_state(&mut ros1_state_val).await;
                        self.report_bridge_statistics(&locked);
                    }

                    async_std::task::sleep({
                        if smth_changed {
                            core::time::Duration::from_millis(100)
                        } else {
                            core::time::Duration::from_millis(200)
                        }
                    })
                    .await;
                }
                Err(e) => {
                    error!("Error reading ROS state: {}", e);

                    self.transit_ros_status(RosStatus::Error);
                    {
                        let mut locked = bridges.lock().await;
                        Self::cleanup(&mut locked);
                        self.report_bridge_statistics(&locked);
                    }
                    async_std::task::sleep(core::time::Duration::from_millis(500)).await;
                }
            }
        }
    }

    // PRIVATE
    fn transit_ros_status(&mut self, new_ros_status: RosStatus) {
        if self.ros_status != new_ros_status {
            self.ros_status = new_ros_status;
            (self.ros_status_callback)(self.ros_status);
        }
    }

    fn report_bridge_statistics(&self, locked: &BridgesStorage) {
        (self.statistics_callback)(locked.status());
    }

    fn cleanup(locked: &mut BridgesStorage) {
        locked.clear();
    }
}
