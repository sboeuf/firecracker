// Copyright 2019 Intel Corporation. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::result;

use futures::sync::oneshot;
use hyper::Method;

use request::{IntoParsedRequest, ParsedRequest};
use vmm::vmm_config::fs::FsDeviceConfig;
use vmm::VmmAction;

impl IntoParsedRequest for FsDeviceConfig {
    fn into_parsed_request(
        self,
        _id_from_path: Option<String>,
        _: Method,
    ) -> result::Result<ParsedRequest, String> {
        let (sender, receiver) = oneshot::channel();
        Ok(ParsedRequest::Sync(
            VmmAction::InsertFsDevice(self, sender),
            receiver,
        ))
    }
}
