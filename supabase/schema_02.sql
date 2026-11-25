
-- Session log gains a channel and an author.
alter table copilot_sessions add column if not exists kind   text not null default 'frame';
alter table copilot_sessions add column if not exists sender text not null default 'copilot';

-- Chat rows carry no image.
alter table copilot_sessions alter column image_url drop not null;

create index if not exists copilot_sessions_thread_idx
  on copilot_sessions (session_code, created_at);
create index if not exists copilot_sessions_kind_idx
  on copilot_sessions (session_code, kind);

-- Session lifecycle. One row per session; 'open' | 'closed'.
create table if not exists copilot_control (
  session_code text primary key,
  state        text not null default 'open',
  closed_at    timestamptz,
  updated_at   timestamptz not null default now()
);

alter table copilot_sessions enable row level security;
alter table copilot_control  enable row level security;

drop policy if exists sessions_anon_select on copilot_sessions;
drop policy if exists sessions_anon_insert on copilot_sessions;
create policy sessions_anon_select on copilot_sessions for select using (true);
create policy sessions_anon_insert on copilot_sessions for insert with check (true);

drop policy if exists control_anon_select on copilot_control;
drop policy if exists control_anon_insert on copilot_control;
drop policy if exists control_anon_update on copilot_control;
create policy control_anon_select on copilot_control for select using (true);
create policy control_anon_insert on copilot_control for insert with check (true);
create policy control_anon_update on copilot_control for update using (true) with check (true);

do $$
begin
  begin
    alter publication supabase_realtime add table copilot_sessions;
  exception when duplicate_object then null;
  end;
  begin
    alter publication supabase_realtime add table copilot_control;
  exception when duplicate_object then null;
  end;
end $$;