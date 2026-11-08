-- Bound calibration work before joining each estimate to its closing edge.
CREATE INDEX events_work_estimates ON events(project, seq)
    WHERE kind = 'card'
      AND CASE WHEN json_valid(payload) THEN json_extract(payload, '$.change') END = 'eta';
CREATE INDEX events_work_edges ON events(card, seq)
    WHERE kind = 'card'
      AND CASE WHEN json_valid(payload) THEN json_extract(payload, '$.change') END IN ('eta', 'moved');
