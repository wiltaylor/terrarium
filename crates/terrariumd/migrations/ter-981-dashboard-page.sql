-- Keep the old rows for rollback, but all reads and writes now use project_pages.
-- Prefix the former fixed panels, then pack saved blocks in their saved order.
INSERT INTO project_pages(project,slug,title,position,widgets,updated_at,updated_by)
SELECT p.name,'dashboard','Dashboard',0,
  '[{"id":"system-actions","kind":"actions","x":0,"y":0,"w":12,"h":2},{"id":"system-summary","kind":"monitor_summary","x":0,"y":2,"w":12,"h":2},{"id":"system-jobs","kind":"job_queue","x":0,"y":4,"w":12,"h":5},{"id":"system-eta","kind":"eta_accuracy","x":0,"y":9,"w":12,"h":5}]',
  COALESCE(d.updated_at,p.updated_at),COALESCE(d.updated_by,'migration')
FROM projects p LEFT JOIN dashboards d ON d.project=p.name;

-- Build the array without depending on JSON aggregate row ordering.
WITH RECURSIVE blocks AS (
  SELECT d.project,CAST(j.key AS INTEGER) n,j.value block,
    CASE json_extract(j.value,'$.size') WHEN 'm' THEN 6 WHEN 'l' THEN 12 ELSE 3 END w
  FROM dashboards d,json_each(d.layout,'$.blocks') j
), packed(project,n,widgets,next_x,next_y) AS (
  SELECT project,-1,widgets,0,14 FROM project_pages WHERE slug='dashboard'
  UNION ALL
  SELECT b.project,b.n,json_insert(p.widgets,'$[#]',json_remove(json_set(b.block,
    '$.kind',CASE json_extract(b.block,'$.kind') WHEN 'note' THEN 'markdown' ELSE json_extract(b.block,'$.kind') END,
    '$.x',CASE WHEN p.next_x+b.w>12 THEN 0 ELSE p.next_x END,
    '$.y',CASE WHEN p.next_x+b.w>12 THEN p.next_y+4 ELSE p.next_y END,
    '$.w',b.w,'$.h',4),'$.size')),
    CASE WHEN p.next_x+b.w>12 THEN b.w ELSE p.next_x+b.w END,
    CASE WHEN p.next_x+b.w>12 THEN p.next_y+4 ELSE p.next_y END
  FROM packed p JOIN blocks b ON b.project=p.project AND b.n=p.n+1
)
UPDATE project_pages SET widgets=(SELECT widgets FROM packed WHERE project=project_pages.project ORDER BY n DESC LIMIT 1)
WHERE slug='dashboard';

CREATE TRIGGER project_default_dashboard AFTER INSERT ON projects BEGIN
  INSERT INTO project_pages(project,slug,title,position,widgets,updated_at,updated_by)
  VALUES(NEW.name,'dashboard','Dashboard',0,
    '[{"id":"system-actions","kind":"actions","x":0,"y":0,"w":12,"h":2},{"id":"system-summary","kind":"monitor_summary","x":0,"y":2,"w":12,"h":2},{"id":"system-jobs","kind":"job_queue","x":0,"y":4,"w":12,"h":5},{"id":"system-eta","kind":"eta_accuracy","x":0,"y":9,"w":12,"h":5}]',NEW.updated_at,'system');
END;
