#[derive(Debug, Eq, PartialEq)]
pub(crate) enum QueueDirection {
    Rx,
    Tx,
}

#[must_use]
pub(crate) fn port_id_to_queue_idx(queue_direction: QueueDirection, port_id: usize) -> usize {
    match queue_direction {
        QueueDirection::Rx if port_id == 0 => 0,
        QueueDirection::Rx => 2 + 2 * port_id,
        QueueDirection::Tx if port_id == 0 => 1,
        QueueDirection::Tx => 2 + 2 * port_id + 1,
    }
}

#[must_use]
pub(crate) fn queue_idx_to_port_id(queue_index: usize) -> (QueueDirection, usize) {
    let port_id = match queue_index {
        0 | 1 => 0,
        2 | 3 => {
            panic!("Invalid argument: {queue_index} is not a valid receiveq nor transmitq index!")
        }
        _ => queue_index / 2 - 1,
    };

    let direction = if queue_index.is_multiple_of(2) {
        QueueDirection::Rx
    } else {
        QueueDirection::Tx
    };

    (direction, port_id)
}

pub(crate) fn num_queues(num_ports: usize) -> usize {
    // 2 control queues and then an rx and tx queue for each port
    2 + 2 * num_ports
}

#[cfg(kani)]
mod verification {
    use super::*;

    /// Prove the port/queue mapping is a bijection: for any valid port_id and
    /// direction, decoding the encoded queue index returns the original (dir, id).
    ///
    /// Valid port IDs: 0 is the control port (always valid); ports >= 1 occupy
    /// queue slots starting at index 4, so the index must not land in 2 or 3
    /// (the control queues). We constrain port_id to a small concrete bound so
    /// Kani's model checker remains tractable.
    #[kani::proof]
    fn proof_port_queue_roundtrip() {
        // Symbolic port_id constrained to [0, 14] (keeps queue indices in u8 range)
        let port_id: usize = kani::any_where(|&id: &usize| id <= 14);
        let is_rx: bool = kani::any();

        let dir = if is_rx {
            QueueDirection::Rx
        } else {
            QueueDirection::Tx
        };

        let q = port_id_to_queue_idx(dir, port_id);
        let (decoded_dir, decoded_port) = queue_idx_to_port_id(q);

        let expected_dir = if is_rx {
            QueueDirection::Rx
        } else {
            QueueDirection::Tx
        };

        kani::assert(
            decoded_port == port_id,
            "port_id must survive queue index encode/decode roundtrip",
        );
        kani::assert(
            decoded_dir == expected_dir,
            "direction must survive queue index encode/decode roundtrip",
        );
        kani::cover!(port_id == 0 && is_rx, "control port Rx roundtrip exercised");
        kani::cover!(port_id == 14 && !is_rx, "max port Tx roundtrip exercised");
    }

    /// Prove that for any valid port_id, the resulting queue index is strictly
    /// less than num_queues(max_ports) where max_ports > port_id.
    #[kani::proof]
    fn proof_queue_idx_in_bounds() {
        let port_id: usize = kani::any_where(|&id: &usize| id <= 14);
        // max_ports is any value strictly greater than port_id
        let max_ports: usize = kani::any_where(|&m: &usize| m > port_id && m <= 15);
        let is_rx: bool = kani::any();

        let dir = if is_rx {
            QueueDirection::Rx
        } else {
            QueueDirection::Tx
        };

        let q = port_id_to_queue_idx(dir, port_id);
        let bound = num_queues(max_ports);

        kani::assert(
            q < bound,
            "port_id_to_queue_idx must return an index within num_queues(max_ports)",
        );
        kani::cover!(port_id == 0, "control port in-bounds exercised");
        kani::cover!(
            port_id == max_ports - 1,
            "last valid port in-bounds exercised"
        );
    }

    /// Prove that calling queue_idx_to_port_id with a control-queue index (2 or 3)
    /// always panics, as specified by the function contract.
    ///
    /// Using `#[kani::should_panic]`: Kani verifies that every execution path
    /// through the function with these inputs reaches a panic.
    #[kani::proof]
    #[kani::should_panic]
    fn proof_control_queue_panics() {
        let idx: usize = kani::any_where(|&i| i == 2 || i == 3);
        let _ = queue_idx_to_port_id(idx);
    }

    /// Prove that queue indices 2 and 3 (the control queues) are never returned
    /// by port_id_to_queue_idx for any port_id.
    ///
    /// Index 0 → port 0 Rx, index 1 → port 0 Tx, and from index 4 onward every
    /// port gets a consecutive (even Rx, odd Tx) pair.  Indices 2 and 3 are
    /// permanently reserved for the control receive/transmit queues.
    #[kani::proof]
    fn proof_control_queues_excluded() {
        let port_id: usize = kani::any_where(|&id: &usize| id <= 14);
        let is_rx: bool = kani::any();

        let dir = if is_rx {
            QueueDirection::Rx
        } else {
            QueueDirection::Tx
        };

        let q = port_id_to_queue_idx(dir, port_id);

        kani::assert(
            q != 2,
            "queue index 2 (control Rx) must never be returned for a data port",
        );
        kani::assert(
            q != 3,
            "queue index 3 (control Tx) must never be returned for a data port",
        );
        kani::cover!(port_id == 0 && is_rx, "control port Rx exclusion exercised");
        kani::cover!(port_id == 14 && !is_rx, "max port Tx exclusion exercised");
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_port_id_to_queue_idx() {
        assert_eq!(port_id_to_queue_idx(QueueDirection::Rx, 0), 0);
        assert_eq!(port_id_to_queue_idx(QueueDirection::Tx, 0), 1);
        assert_eq!(port_id_to_queue_idx(QueueDirection::Rx, 1), 4);
        assert_eq!(port_id_to_queue_idx(QueueDirection::Tx, 1), 5);
    }

    #[test]
    fn test_queue_idx_to_port_id_ok() {
        assert_eq!(queue_idx_to_port_id(0), (QueueDirection::Rx, 0));
        assert_eq!(queue_idx_to_port_id(1), (QueueDirection::Tx, 0));
        assert_eq!(queue_idx_to_port_id(4), (QueueDirection::Rx, 1));
        assert_eq!(queue_idx_to_port_id(5), (QueueDirection::Tx, 1));
        assert_eq!(queue_idx_to_port_id(6), (QueueDirection::Rx, 2));
        assert_eq!(queue_idx_to_port_id(7), (QueueDirection::Tx, 2));
    }

    #[test]
    #[should_panic]
    fn test_queue_idx_to_port_id_panic_rx_control() {
        let _ = queue_idx_to_port_id(2);
    }

    #[test]
    #[should_panic]
    fn test_queue_idx_to_port_id_panic_tx_control() {
        let _ = queue_idx_to_port_id(3);
    }
}
