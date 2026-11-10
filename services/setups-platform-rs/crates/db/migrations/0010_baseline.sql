--
-- PostgreSQL database dump
--

\restrict mU2MAGKOddTmzwYzeNRkdIVK83MVdQT84uQF52P8idugdWugUBEgX7mi7gAlzTY

-- Dumped from database version 17.11
-- Dumped by pg_dump version 17.11

SET statement_timeout = 0;
SET lock_timeout = 0;
SET idle_in_transaction_session_timeout = 0;
SET transaction_timeout = 0;
SET client_encoding = 'UTF8';
SET standard_conforming_strings = on;
SELECT pg_catalog.set_config('search_path', '', false);
SET check_function_bodies = false;
SET xmloption = content;
SET client_min_messages = warning;
SET row_security = off;

SET default_tablespace = '';

SET default_table_access_method = heap;

--
-- Name: analytics_matrix_cache; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.analytics_matrix_cache (
    scope_key text NOT NULL,
    direction text NOT NULL,
    maturity_tenths integer NOT NULL,
    month_key text NOT NULL,
    tp_pct numeric NOT NULL,
    sl_pct numeric NOT NULL,
    horizon_minutes integer NOT NULL,
    time_shift_minutes integer NOT NULL,
    price_change_ppm integer NOT NULL,
    algorithm_version integer NOT NULL,
    result_json jsonb NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    cluster_gap_minutes integer DEFAULT '-1'::integer NOT NULL
);


--
-- Name: analytics_selection_cache; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.analytics_selection_cache (
    scope_key text NOT NULL,
    maturity_tenths integer NOT NULL,
    month_key text NOT NULL,
    algorithm_version integer NOT NULL,
    result_json jsonb NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    cluster_gap_minutes integer DEFAULT '-1'::integer NOT NULL
);


--
-- Name: analytics_trades_cache; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.analytics_trades_cache (
    scope_key text NOT NULL,
    direction text NOT NULL,
    maturity_tenths integer NOT NULL,
    exchange_code text NOT NULL,
    category text NOT NULL,
    symbol text NOT NULL,
    close_time_unix bigint NOT NULL,
    tp_pct numeric NOT NULL,
    sl_pct numeric NOT NULL,
    horizon_minutes integer NOT NULL,
    time_shift_minutes integer NOT NULL,
    price_change_ppm integer NOT NULL,
    algorithm_version integer NOT NULL,
    result_json jsonb NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    cluster_gap_minutes integer DEFAULT '-1'::integer NOT NULL
);


--
-- Name: instrument_lists; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.instrument_lists (
    id integer NOT NULL,
    list_type text NOT NULL,
    exchange_code text NOT NULL,
    category text NOT NULL,
    symbol text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: instrument_lists_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

ALTER TABLE public.instrument_lists ALTER COLUMN id ADD GENERATED ALWAYS AS IDENTITY (
    SEQUENCE NAME public.instrument_lists_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1
);


--
-- Name: live_setups; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.live_setups (
    id integer NOT NULL,
    exchange_code text NOT NULL,
    category text NOT NULL,
    symbol text NOT NULL,
    algorithm_id text NOT NULL,
    algorithm_version text NOT NULL,
    params jsonb NOT NULL,
    open_time timestamp with time zone NOT NULL,
    close_time timestamp with time zone NOT NULL,
    close_price text NOT NULL,
    notified_at timestamp with time zone,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    details jsonb
);


--
-- Name: live_setups_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

ALTER TABLE public.live_setups ALTER COLUMN id ADD GENERATED ALWAYS AS IDENTITY (
    SEQUENCE NAME public.live_setups_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1
);


--
-- Name: push_subscriptions; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.push_subscriptions (
    endpoint text NOT NULL,
    p256dh text NOT NULL,
    auth text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: replay_batches; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.replay_batches (
    id integer NOT NULL,
    exchange_code text NOT NULL,
    category text NOT NULL,
    algorithm_id text NOT NULL,
    algorithm_version text,
    params jsonb NOT NULL,
    from_ts timestamp with time zone NOT NULL,
    to_ts timestamp with time zone NOT NULL,
    instrument_count integer NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: replay_batches_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

CREATE SEQUENCE public.replay_batches_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: replay_batches_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -
--

ALTER SEQUENCE public.replay_batches_id_seq OWNED BY public.replay_batches.id;


--
-- Name: replay_cache; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.replay_cache (
    cache_key text NOT NULL,
    exchange_code text NOT NULL,
    category text NOT NULL,
    symbol text NOT NULL,
    algorithm_id text NOT NULL,
    algorithm_version text NOT NULL,
    params jsonb NOT NULL,
    from_ts timestamp with time zone NOT NULL,
    to_ts timestamp with time zone NOT NULL,
    setups jsonb NOT NULL,
    warmup_minutes integer NOT NULL,
    processed_minutes integer NOT NULL,
    computed_at timestamp with time zone DEFAULT now() NOT NULL,
    quality jsonb,
    quality_source_run_id integer
);


--
-- Name: replay_quality_intervals; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.replay_quality_intervals (
    run_id integer NOT NULL,
    from_unix bigint NOT NULL,
    to_unix bigint NOT NULL,
    reason text NOT NULL,
    CONSTRAINT replay_quality_positive_interval CHECK ((to_unix > from_unix))
);


--
-- Name: replay_runs; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.replay_runs (
    id integer NOT NULL,
    exchange_code text NOT NULL,
    category text NOT NULL,
    symbol text NOT NULL,
    from_ts timestamp with time zone NOT NULL,
    to_ts timestamp with time zone NOT NULL,
    algorithm_id text NOT NULL,
    algorithm_version text NOT NULL,
    params jsonb NOT NULL,
    status text DEFAULT 'queued'::text NOT NULL,
    cache_hit boolean DEFAULT false NOT NULL,
    setups jsonb,
    warmup_minutes integer,
    processed_minutes integer,
    error text,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    quality jsonb,
    quality_source_run_id integer,
    quality_computed_at timestamp with time zone,
    batch_id integer
);


--
-- Name: replay_runs_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

ALTER TABLE public.replay_runs ALTER COLUMN id ADD GENERATED ALWAYS AS IDENTITY (
    SEQUENCE NAME public.replay_runs_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1
);


--
-- Name: settings; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.settings (
    key text NOT NULL,
    value jsonb NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: replay_batches id; Type: DEFAULT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.replay_batches ALTER COLUMN id SET DEFAULT nextval('public.replay_batches_id_seq'::regclass);


--
-- Name: analytics_matrix_cache analytics_matrix_cache_pk; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.analytics_matrix_cache
    ADD CONSTRAINT analytics_matrix_cache_pk PRIMARY KEY (scope_key, direction, maturity_tenths, cluster_gap_minutes, month_key, tp_pct, sl_pct, horizon_minutes, time_shift_minutes, price_change_ppm, algorithm_version);


--
-- Name: analytics_selection_cache analytics_selection_cache_pk; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.analytics_selection_cache
    ADD CONSTRAINT analytics_selection_cache_pk PRIMARY KEY (scope_key, maturity_tenths, cluster_gap_minutes, month_key, algorithm_version);


--
-- Name: analytics_trades_cache analytics_trades_cache_pk; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.analytics_trades_cache
    ADD CONSTRAINT analytics_trades_cache_pk PRIMARY KEY (scope_key, direction, maturity_tenths, cluster_gap_minutes, exchange_code, category, symbol, close_time_unix, tp_pct, sl_pct, horizon_minutes, time_shift_minutes, price_change_ppm, algorithm_version);


--
-- Name: instrument_lists instrument_lists_identity_unique; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.instrument_lists
    ADD CONSTRAINT instrument_lists_identity_unique UNIQUE (list_type, exchange_code, category, symbol);


--
-- Name: instrument_lists instrument_lists_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.instrument_lists
    ADD CONSTRAINT instrument_lists_pkey PRIMARY KEY (id);


--
-- Name: live_setups live_setups_identity_unique; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.live_setups
    ADD CONSTRAINT live_setups_identity_unique UNIQUE (exchange_code, category, symbol, algorithm_id, algorithm_version, open_time);


--
-- Name: live_setups live_setups_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.live_setups
    ADD CONSTRAINT live_setups_pkey PRIMARY KEY (id);


--
-- Name: push_subscriptions push_subscriptions_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.push_subscriptions
    ADD CONSTRAINT push_subscriptions_pkey PRIMARY KEY (endpoint);


--
-- Name: replay_batches replay_batches_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.replay_batches
    ADD CONSTRAINT replay_batches_pkey PRIMARY KEY (id);


--
-- Name: replay_cache replay_cache_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.replay_cache
    ADD CONSTRAINT replay_cache_pkey PRIMARY KEY (cache_key);


--
-- Name: replay_quality_intervals replay_quality_intervals_pk; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.replay_quality_intervals
    ADD CONSTRAINT replay_quality_intervals_pk PRIMARY KEY (run_id, from_unix);


--
-- Name: replay_runs replay_runs_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.replay_runs
    ADD CONSTRAINT replay_runs_pkey PRIMARY KEY (id);


--
-- Name: settings settings_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.settings
    ADD CONSTRAINT settings_pkey PRIMARY KEY (key);


--
-- Name: replay_cache_quality_source_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX replay_cache_quality_source_idx ON public.replay_cache USING btree (quality_source_run_id);


--
-- Name: replay_runs_batch_id_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX replay_runs_batch_id_idx ON public.replay_runs USING btree (batch_id);


--
-- Name: replay_runs_quality_source_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX replay_runs_quality_source_idx ON public.replay_runs USING btree (quality_source_run_id);


--
-- Name: replay_cache replay_cache_quality_source_run_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.replay_cache
    ADD CONSTRAINT replay_cache_quality_source_run_id_fkey FOREIGN KEY (quality_source_run_id) REFERENCES public.replay_runs(id) ON DELETE SET NULL;


--
-- Name: replay_quality_intervals replay_quality_intervals_run_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.replay_quality_intervals
    ADD CONSTRAINT replay_quality_intervals_run_id_fkey FOREIGN KEY (run_id) REFERENCES public.replay_runs(id) ON DELETE CASCADE;


--
-- Name: replay_runs replay_runs_batch_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.replay_runs
    ADD CONSTRAINT replay_runs_batch_id_fkey FOREIGN KEY (batch_id) REFERENCES public.replay_batches(id) ON DELETE CASCADE;


--
-- Name: replay_runs replay_runs_quality_source_run_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.replay_runs
    ADD CONSTRAINT replay_runs_quality_source_run_id_fkey FOREIGN KEY (quality_source_run_id) REFERENCES public.replay_runs(id) ON DELETE SET NULL;


--
-- PostgreSQL database dump complete
--

\unrestrict mU2MAGKOddTmzwYzeNRkdIVK83MVdQT84uQF52P8idugdWugUBEgX7mi7gAlzTY

