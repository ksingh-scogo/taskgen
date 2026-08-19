# Scogo Sovereign Enterprise NetOps Taskgen and ATIF Design

Date: 2026-08-19

Status: approved in chat; written-spec review pending

Branch: `codex/netops-taxonomy-atif` from `origin/master`

## 1. Outcome

Taskgen will gain a dedicated compositional Enterprise NetOps taxonomy, prompt files for prompt-seed generation and teacher rollout generation, strict schemas for the task, audit, and SFT records, and ATIF v1.7 import/export.

The pipeline boundary is:

```text
Taskgen prompt seeds
  -> teacher rollout generation
  -> harness-owned tool execution and state capture
  -> independent verification and safety grading
  -> canonical audit trajectories
  -> accepted SFT projection
  -> optional ATIF v1.7 import/export
```

Taskgen remains a prompt-seed generator. It must not fabricate tool results, live state, approvals, ground truth, state hashes, verification outcomes, rewards, or grader decisions.

## 2. Scope

Included:

- enterprise, campus, branch, data-center, cloud, hybrid, multicloud, Kubernetes, remote-access, edge, OT/IoT, AI/HPC, and real-time enterprise networks;
- physical links, Ethernet, IP, routing, switching, DNS, DHCP, Wi-Fi, WAN, SD-WAN, VPN, firewalls, NAC, load balancers, cloud networking, container networking, observability, automation, lifecycle, resilience, and safe change behavior;
- vendor-neutral, single-vendor, and multi-vendor tasks;
- read-only investigation, evidence interpretation, tool selection, configuration and IaC review, bounded remediation plans, approval-gated changes, post-change verification, rollback, abstention, and escalation;
- local/on-premises teacher and student workflows with no dependency on a hosted model at inference time;
- ATIF v1.7 as an interchange representation for completed teacher trajectories.

Excluded:

- 3GPP RAN, EPC, 5GC, IMS, mobile spectrum planning, carrier OSS/BSS, carrier optical backbone, and service-provider core operations;
- certification-question generation as a primary dataset objective;
- live network mutation by Taskgen;
- teacher-authored tool results, approvals, ground truth, state hashes, verification, safety grades, acceptance decisions, or rewards;
- copying hidden chain-of-thought into the SFT dataset;
- using ATIF as the canonical Scogo audit schema.

Enterprise use of LTE/5G as a WAN underlay is included. Operating a mobile carrier network is excluded.

## 3. Repository layout

Implementation will produce this layout:

```text
Cargo.toml
README.md
docs/
  it-ops-taxonomy.yaml
  netops-taxonomy.yaml
  netops-data-contract.md
prompts/
  netops-taskgen-system-v1.txt
  netops-teacher-system-v1.txt
schemas/
  netops-task-v1.schema.json
  netops-teacher-trajectory-audit-v1.schema.json
  netops-teacher-trajectory-sft-v1.schema.json
src/
  main.rs
  atif.rs
  schema.rs
  taxonomy.rs
tests/
  fixtures/
    atif-v1.7/
    canonical/
    taxonomy/
```

`docs/it-ops-taxonomy.yaml` remains the general IT Ops source. `docs/netops-taxonomy.yaml` is an independent source for the NetOps model and does not replace or extend the `network` category in the general file.

Both YAML files use `schema_version: scogo.taskgen.taxonomy.v1`. The general file uses `kind: hierarchical`; the NetOps file uses `kind: compositional`. Taskgen parses both at runtime with the same validated loader. When `--taxonomy` is omitted, `docs/it-ops-taxonomy.yaml` is embedded in the binary with `include_str!`. The generated Rust `DOMAINS` and `DEFAULT_DISTRIBUTION` catalogs and `scripts/codegen_domains.py` are removed so YAML is the only taxonomy source of truth.

## 4. Command-line contract

Taskgen will use explicit subcommands:

```text
taskgen generate [generation options]
taskgen atif export [conversion options]
taskgen atif import [conversion options]
taskgen taxonomy validate --taxonomy <file>
```

Generation:

```bash
taskgen generate \
  --taxonomy docs/netops-taxonomy.yaml \
  --system-prompt-file prompts/netops-taskgen-system-v1.txt \
  --api-key "$OPENAI_API_KEY" \
  --model scogoai/gpt-5.6-luna-max \
  --count 1000 \
  --output data/netops-prompts.jsonl
```

New generation flags:

| Flag | Contract |
|---|---|
| `--taxonomy <FILE>` | Load and validate a runtime taxonomy. When omitted, use the embedded general IT Ops taxonomy. |
| `--system-prompt-file <FILE>` | Read the complete system prompt as UTF-8. Mutually exclusive with `--system-prompt`. |
| `--seed <U64>` | Optional reproducible coordinate sampling seed. It does not make remote model output deterministic. |

System-prompt precedence is:

1. `--system-prompt`;
2. `--system-prompt-file`;
3. `defaults.system_prompt_file` from the selected taxonomy, resolved relative to the taxonomy file;
4. the built-in general Taskgen prompt.

The current product-specific OEM addendum applies only to the embedded general IT Ops taxonomy. It is never appended to the NetOps prompt because vendor scope is an explicit NetOps coordinate.

ATIF conversion:

```bash
taskgen atif export \
  --input data/netops-teacher-trajectories.audit.v1.jsonl \
  --output data/netops-teacher-trajectories.atif.v1.7.jsonl

taskgen atif import \
  --input data/external.atif.v1.7.jsonl \
  --output data/external.audit.v1.jsonl
```

Both commands accept `--container json` for one object or `--container jsonl` for one complete object per line. The default is inferred from `.json` or `.jsonl`; any other extension requires the flag.

`taskgen taxonomy validate` performs structural and semantic validation without an API key or model call.

## 5. Compositional task coordinates

Each NetOps prompt is sampled from:

```text
domain
  + subdomain
  + task_family
  + environment
  + vendor_scope and vendor/platform selection
  + incident_mechanism
  + evidence_condition
  + evidence_bundle
  + action_risk
  + difficulty
  + presentation
```

Sampling order is deterministic for a given PRNG seed:

1. domain;
2. subdomain within the domain;
3. task family;
4. environment filtered by the domain allow-list;
5. vendor scope, then zero, one, or two vendors from domain-allowed vendor groups;
6. incident mechanism filtered by the domain allow-list;
7. evidence condition;
8. evidence bundle filtered by the domain allow-list;
9. action risk;
10. difficulty using the default distribution filtered and reweighted by the task-family bounds;
11. presentation.

Every complete default distribution must contain finite, non-negative values and sum to `1.0 +/- 0.000001`. Taskgen does not silently normalize a malformed complete distribution. After a domain allow-list filters an otherwise valid global axis, the eligible subset is intentionally renormalized for sampling. Every enabled domain must have at least one enabled subdomain and one eligible choice for every filtered axis. Duplicate IDs are errors. Unknown references are errors.

Subdomains are uniform within their domain in v1 unless an explicit `weight` is present. This avoids unsupported precision while making later expert-driven rebalancing possible. A subdomain ID is scoped to its parent domain; the stable unique key is `domain_id/subdomain_id`. Duplicate subdomain IDs within one domain are errors, while a deliberately shared term such as `ipam` in two different domains is valid.

## 6. NetOps taxonomy file contract

`docs/netops-taxonomy.yaml` uses this structure:

```yaml
schema_version: scogo.taskgen.taxonomy.v1
id: scogo-enterprise-netops-v1
kind: compositional
label: Scogo Sovereign Enterprise NetOps

scope:
  include: [...]
  exclude: [...]

defaults:
  system_prompt_file: ../prompts/netops-taskgen-system-v1.txt
  difficulty_distribution: {...}

vendor_groups:
  - id: routing_switching
    weight: 1.0
    vendors:
      - id: cisco_ios_xe
        label: Cisco IOS XE
        weight: 1.0

axes:
  domains:
    - id: l3_routing
      label: Layer 3 Routing
      weight: 0.07
      vendor_groups: [routing_switching, open_networking]
      environments: [campus, branch, data_center, hybrid, cloud, lab_staging]
      incident_mechanisms: [misconfiguration, protocol_state_failure, asymmetric_path, convergence_failure, software_defect, multi_fault]
      evidence_bundles: [config_oper_state, routing_tables, topology_state, packet_capture, multi_source, intentionally_missing]
      subdomains:
        - id: bgp_route_leak
          label: BGP route leak
        - id: ospf_adjacency
          label: OSPF adjacency

  task_families: [...]
  environments: [...]
  vendor_scopes: [...]
  incident_mechanisms: [...]
  evidence_conditions: [...]
  evidence_bundles: [...]
  action_risks: [...]
  presentations: [...]
```

Every axis option has `id`, `label`, `weight`, and an optional `enabled` boolean. Task families additionally have `difficulty_min`, `difficulty_max`, and optional per-level `difficulty_multiplier`.

For `kind: hierarchical`, `axes.domains` is replaced by the existing `categories` list, with each category carrying its default `weight`. A category contains domains and subdomains exactly as the current general IT Ops taxonomy does. `--distribution` addresses category IDs for a hierarchical taxonomy and domain IDs for a compositional taxonomy.

### 6.1 Domain distribution

| Domain | Weight |
|---|---:|
| `physical_link_hardware` | 0.02 |
| `packet_protocol_foundations` | 0.01 |
| `layer2_switching` | 0.05 |
| `ip_addressing_ipam` | 0.03 |
| `layer3_routing` | 0.07 |
| `wan_internet_edge` | 0.05 |
| `sdwan` | 0.04 |
| `wireless_lan` | 0.05 |
| `network_services` | 0.06 |
| `vpn_remote_access` | 0.04 |
| `firewall_network_security` | 0.07 |
| `nac_zero_trust` | 0.04 |
| `application_delivery` | 0.04 |
| `datacenter_fabric` | 0.03 |
| `cloud_hybrid_networking` | 0.07 |
| `container_kubernetes_networking` | 0.05 |
| `sdn_network_virtualization` | 0.03 |
| `network_performance_qos` | 0.04 |
| `network_resilience` | 0.03 |
| `network_observability` | 0.06 |
| `network_automation` | 0.05 |
| `network_lifecycle_governance` | 0.02 |
| `storage_hpc_ai_networking` | 0.02 |
| `ot_iot_networking` | 0.02 |
| `enterprise_realtime_networking` | 0.01 |

The weights sum to 1.00. They emphasize high-volume operational surfaces without allowing observability, automation, safety, or newer AI/OT fabrics to disappear.

### 6.2 Task-family distribution

| Task family | Weight | Difficulty |
|---|---:|---:|
| `troubleshooting_rca` | 0.25 | 2-10 |
| `telemetry_config_log_interpretation` | 0.20 | 1-10 |
| `tool_selection_next_best_action` | 0.15 | 2-10 |
| `config_iac_review_repair` | 0.15 | 3-10 |
| `change_approval_verification_rollback` | 0.10 | 4-10 |
| `abstention_uncertainty_escalation` | 0.10 | 3-10 |
| `architecture_capacity_migration_optimization` | 0.05 | 5-10 |

Certification recall is not a task family. A prompt may require protocol knowledge, but the operator must apply it to evidence, a decision, or a safe plan.

### 6.3 Difficulty distribution

```yaml
difficulty_distribution:
  1: 0.02
  2: 0.03
  3: 0.08
  4: 0.12
  5: 0.18
  6: 0.18
  7: 0.15
  8: 0.12
  9: 0.07
  10: 0.05
```

Difficulty semantics:

- 1-3: one device or service, clear symptom, low ambiguity, low risk, existing runbook;
- 4-6: multiple plausible causes, more than one evidence source, multiple devices or layers, moderate operational risk;
- 7-8: multi-device, cross-layer or multi-vendor, conflicting evidence, production constraints, approval, blast-radius, rollback, or freeze concerns;
- 9-10: cross-domain or multi-fault, partial observability, long causal chain, stale state, safety conflict, absent runbook, or a correct need to abstain, stage, or escalate.

### 6.4 Other default distributions

Environment:

```yaml
campus: 0.14
branch: 0.10
data_center: 0.12
hybrid: 0.12
cloud: 0.10
multicloud: 0.06
kubernetes: 0.08
remote_access: 0.06
enterprise_wireless: 0.07
ot: 0.04
ai_hpc: 0.03
edge: 0.04
lab_staging: 0.04
```

Vendor scope:

```yaml
vendor_neutral: 0.35
single_vendor: 0.45
multi_vendor: 0.20
```

Evidence condition:

```yaml
sufficient: 0.45
partial: 0.30
contradictory: 0.10
stale: 0.05
missing_live_state: 0.10
```

Action risk:

```yaml
read_only_investigation: 0.55
advisory_plan_only: 0.20
approval_gated_change: 0.15
staging_or_simulation: 0.07
emergency_change_decision: 0.03
```

Presentation:

```yaml
incident_ticket: 0.18
oncall_chat: 0.18
cli_ssh_session: 0.16
config_review: 0.12
change_request: 0.10
war_room: 0.10
audit_review: 0.06
architecture_review: 0.06
api_automation_brief: 0.04
```

### 6.5 Exact domain and subdomain inventory

The initial taxonomy contains the following stable IDs. Labels in the YAML are human-readable; IDs are the data contract.

```yaml
physical_link_hardware:
  [copper_cabling, fiber_cabling, optics_transceivers, dac_aoc, patch_panels,
   interface_errors, crc_fcs_errors, duplex_speed_mismatch, autonegotiation,
   link_flap, signal_levels_dom, poe_power_budget, port_channel_member_physical,
   breakout_ports, hardware_asic_failure, environmental_thermal_power,
   smartnic_dpu, cabling_documentation]

packet_protocol_foundations:
  [ethernet_frames, arp, ndp, icmp, tcp_handshake_reset_retransmission, udp,
   mtu_fragmentation_pmtud, mss_clamping, ttl_hop_limit, checksums,
   encapsulation_overhead, multicast_broadcast_unknown_unicast,
   qos_markings_dscp_ecn, packet_ordering_duplication, socket_flow_five_tuple]

layer2_switching:
  [vlans, access_ports, trunks, native_vlan, qinq, stp_rstp_mstp,
   loop_guard_root_guard_bpdu_guard, mac_address_table, mac_flap,
   lacp_port_channels, mlag_vpc, storm_control, dhcp_snooping,
   dynamic_arp_inspection, ip_source_guard, private_vlan]

ip_addressing_ipam:
  [ipv4_subnetting, ipv6_addressing, dual_stack, ipam_allocation,
   prefix_utilization, duplicate_ip, anycast_ip, loopback_addressing,
   vrf_address_spaces, rfc1918_overlap, nat_address_planning,
   dhcp_reservations, slaac_router_advertisement, renumbering,
   address_discovery_reconciliation, dns_ipam_source_of_truth]

layer3_routing:
  [static_routes, policy_based_routing, ospf_adjacency, ospf_lsa_area,
   isis_adjacency_database, bgp_session, bgp_best_path,
   route_redistribution, route_filtering_prefix_lists,
   route_maps_policy_statements, bgp_communities,
   localpref_med_aspath, bgp_route_leak, default_route_blackhole, ecmp,
   vrf_lite, route_recursion, routing_convergence_graceful_restart, bfd,
   multicast_routing_pim_igmp, rpki_origin_validation]

wan_internet_edge:
  [isp_peering_transit, dedicated_internet_access, mpls_l3vpn, leased_lines,
   broadband_underlay, lte_5g_backup_underlay, pppoe, bgp_multihoming,
   nat_pat, carrier_handoff, wan_brownout, last_mile_latency_loss,
   asymmetric_internet_path, cgnat_impact, public_ip_asn,
   internet_ddos_handoff, wan_capacity, circuit_failover]

sdwan:
  [overlay_tunnels, underlay_health, control_connections, orchestration,
   application_aware_routing, sla_classes, path_selection,
   segmentation_vrf, policy_conflicts, direct_internet_local_breakout,
   service_chaining, high_availability, site_onboarding_ztp, nat_traversal,
   qos, brownout_remediation, controller_upgrades, telemetry, cloud_onramps]

wireless_lan:
  [rf_planning, coverage_capacity, channel_power_rrm,
   cochannel_adjacent_interference, dfs_events, roaming_80211kvr,
   association_authentication, wpa2_wpa3_enterprise,
   eap_radius_certificates, ssid_vlan_mapping, controller_ap_join, capwap,
   wireless_mesh, captive_portal_guest, band_steering, airtime_fairness,
   client_isolation, wireless_iot, location_rtls, spectrum_analysis,
   wifi_6_6e_7, multicast_broadcast_optimization]

network_services:
  [dns_authoritative, dns_recursive, dns_forwarding, dns_split_horizon,
   dnssec, dns_ttl_cache, doh_dot, ipam, dhcp_dora, dhcp_relay,
   dhcp_failover, dhcp_options, ntp_ptp, radius_tacacs,
   ldap_active_directory_dependency, pki_certificate_lifecycle,
   proxy_pac, cdn_edge_dns, service_discovery, anycast_service_ip]

vpn_remote_access:
  [ipsec_ikev1_ikev2, phase1_phase2_selectors, route_based_vpn,
   policy_based_vpn, site_to_site_vpn, remote_access_vpn, ssl_vpn,
   wireguard, dmvpn, gre_tunnels, ztna_private_access, client_posture,
   split_tunnel, full_tunnel, nat_t, rekey_lifetime, certificate_auth,
   mfa_saml, overlapping_networks, vpn_mtu_mss, vpn_ha, vpn_routing]

firewall_network_security:
  [policy_matching_order, address_service_objects, state_tables,
   snat_dnat, application_identification, url_filtering, tls_inspection,
   ips_ids, zone_based_policy, vrf_vdom_vsys, ha_session_sync,
   asymmetric_routing, rule_shadowing, any_any_rules,
   east_west_segmentation, north_south_policy, egress_filtering,
   geo_ip_reputation, firewall_logging, policy_hit_counts,
   virtual_patching, ddos_policy, cloud_firewall_policy,
   config_change_rollback]

nac_zero_trust:
  [dot1x, mac_auth_bypass, eap_methods, radius, nac_certificates, posture,
   profiling, guest_byod, dynamic_vlan_assignment, downloadable_acl,
   quarantine, change_of_authorization, supplicant,
   switch_ap_integration, identity_sources, fail_open_fail_closed,
   tacacs_device_admin, security_group_tags, microsegmentation,
   iot_onboarding, nac_policy_drift, nac_high_availability]

application_delivery:
  [l4_l7_load_balancing, pools_members, health_monitors,
   balancing_algorithms, persistence_stickiness, tls_termination_reencrypt,
   certificate_chain_sni, http2_http3, reverse_proxy, waf_integration,
   gslb, dns_load_balancing, anycast_delivery, ingress_api_gateway,
   connection_draining, rate_limiting, header_rewriting,
   source_ip_proxy_protocol, websocket, grpc, content_switching,
   autoscaling_capacity, adc_high_availability, config_sync]

datacenter_fabric:
  [leaf_spine, evpn_vxlan_control_plane, l2_l3_vni, anycast_gateway,
   bgp_underlay, evpn_route_types, evpn_multihoming_esi, mlag_vpc,
   fabric_mtu, fabric_ecmp, border_leaf, data_center_interconnect,
   vrf_route_leaking, multicast_replication, arp_suppression,
   endpoint_mobility, fabric_automation_controller, aci_policy,
   storage_fabric_integration, fabric_convergence, fabric_maintenance_upgrade]

cloud_hybrid_networking:
  [vpc_vnet_architecture, cloud_subnets_route_tables,
   security_groups_nsg_nacl, cloud_firewall, internet_nat_gateway,
   vpc_vnet_peering, transit_gateway_vwan_hub_spoke,
   private_link_private_endpoints, cloud_dns, cloud_load_balancer,
   cloud_vpn_gateway, direct_connect_expressroute_interconnect_cen,
   cloud_bgp_route_exchange, overlapping_cloud_cidr,
   multi_account_subscription_project, multicloud_connectivity,
   cloud_egress, network_appliance_insertion, udr_gateway_route_tables,
   cloud_flow_logs, cloud_ipv6, cloud_kubernetes_networking,
   landing_zone_network_policy, multi_region_network_ha]

container_kubernetes_networking:
  [cni_lifecycle, pod_ipam, clusterip_services,
   kube_proxy_iptables_ipvs_nft, networkpolicy, ingress, gateway_api,
   service_loadbalancer, coredns, overlay_underlay, kubernetes_mtu,
   node_to_pod, pod_to_pod, kubernetes_egress_nat, kubernetes_dual_stack,
   service_mesh, sidecar_ambient_mesh, mesh_mtls, multicluster_networking,
   bgp_service_advertisement, cilium_ebpf, calico,
   cloud_cni_prefix_delegation, endpoint_slices, hairpin_hostnetwork,
   kubernetes_network_observability]

sdn_network_virtualization:
  [controller_availability, northbound_southbound_apis, openflow,
   netconf_yang, gnmi, sdn_overlays, vmware_nsx, aci_controller,
   intent_policy, network_virtualization, distributed_routing,
   overlay_gateways, vtep, tenant_isolation,
   controller_state_reconciliation, policy_compilation,
   service_insertion, virtual_switches_ovs, sdn_upgrades,
   controller_telemetry]

network_performance_qos:
  [latency_jitter_loss, throughput_goodput, microbursts, queue_drops,
   bufferbloat, qos_classification_marking, policing, shaping,
   queuing_llq_wfq, congestion_avoidance_wred_ecn, trust_boundaries,
   application_slas, synthetic_probing, tcp_performance,
   udp_realtime_performance, bandwidth_delay_product, capacity_planning,
   oversubscription, elephant_mice_flows, path_mtu_performance,
   wan_optimization, packet_duplication_fec]

network_resilience:
  [ha_pairs, hsrp_vrrp, ecmp_redundancy, link_node_site_failure,
   convergence, graceful_restart_nsf, bfd_failover, stateful_failover,
   split_brain, route_dampening, maintenance_drain, change_rollback,
   chaos_failure_injection, config_backup_restore,
   redundant_power_supervisor, control_plane_quorum,
   disaster_recovery_connectivity, multi_region_failover,
   dependency_mapping, network_slo_error_budget]

network_observability:
  [snmp, streaming_telemetry, gnmi_telemetry, syslog,
   netflow_sflow_ipfix, packet_capture, span_tap, synthetic_tests,
   traceroute_path_analysis, twamp, log_event_correlation,
   lldp_cdp_topology, config_state_diff, baseline_anomaly,
   alert_tuning, event_deduplication, time_sync, network_dashboards,
   network_slo, digital_experience, wireless_telemetry, cloud_flow_logs,
   ebpf_network_observability, telemetry_data_quality,
   multi_source_evidence_correlation]

network_automation:
  [ansible, terraform, nornir_netmiko_napalm, python_api,
   netconf_restconf_gnmi, yang_models, network_gitops, network_cicd,
   netbox_nautobot_source_of_truth, intent_validation,
   prechecks_postchecks, dry_run_diff, config_templates, idempotency,
   transactional_change, config_backup_rollback, secrets_management,
   rbac_approvals, drift_detection, compliance_automation, ztp,
   inventory_automation, batfish_containerlab_pyats,
   workflow_orchestration, automation_retry_failure,
   rate_limit_concurrency, machine_readable_evidence]

network_lifecycle_governance:
  [inventory_cmdb, eol_eos, software_firmware_lifecycle,
   vulnerability_advisories, patch_upgrade, licensing_support,
   configuration_standards, golden_config, compliance_audit,
   change_management_cab, maintenance_windows, backup_restore,
   capacity_forecast, vendor_tac_escalation, rma_spares,
   documentation_diagrams, asset_ownership,
   segmentation_policy_governance, certificate_lifecycle,
   runbook_disaster_readiness]

storage_hpc_ai_networking:
  [iscsi, nvme_tcp, fibre_channel, lossless_ethernet,
   pfc_ets_dcqcn_ecn, roce_v2, rdma, jumbo_frames,
   storage_multipathing, storage_vlan_vrf, ai_leaf_spine,
   gpu_east_west, rail_optimized_fabric, nccl_collectives, infiniband,
   ai_fabric_latency_loss, adaptive_routing, smartnic_dpu,
   gpudirect, ai_fabric_telemetry, benchmark_validation,
   ai_fabric_failure_isolation]

ot_iot_networking:
  [purdue_segmentation, it_ot_dmz, modbus, dnp3, profinet,
   ethernet_ip, bacnet, unmanaged_industrial_switches,
   deterministic_industrial_traffic, legacy_ot_devices,
   serial_gateways, ot_nac_profiling, passive_asset_discovery,
   safety_availability_constraints, remote_vendor_access,
   industrial_firewalls, iot_onboarding, device_identity_certificates,
   mqtt, edge_gateways, ptp_time_sync, industrial_redundancy_rings,
   ot_monitoring, constrained_firmware_change, ot_incident_containment,
   data_diode_air_gap]

enterprise_realtime_networking:
  [voice_vlan, sip_signaling, rtp_media, session_border_controller,
   call_routing, dial_plans, voice_video_qos, jitter_loss_mos,
   enterprise_conferencing, teams_webex_zoom_media_paths,
   contact_center_networking, multicast_video, digital_signage,
   realtime_nat_traversal, emergency_calling,
   call_recording_compliance, webrtc, endpoint_registration,
   codec_transcoding, call_admission_bandwidth, site_survivability]
```

## 7. Prompt files

### 7.1 `prompts/netops-taskgen-system-v1.txt`

The exact prompt is:

```text
You generate prompt seeds for Scogo Sovereign Enterprise NetOps training data.

Output exactly one standalone user task prompt. Do not answer the task. Do not emit metadata, labels, explanations, rubrics, ground truth, tool results, or mention synthetic data.

The purpose is to train operational behavior, not certification recall. The task should require one or more of: investigation, evidence interpretation, hypothesis generation, the next diagnostic action, deterministic tool selection, configuration or infrastructure-as-code review, safe remediation planning, approval-aware change planning, verification, rollback, abstention, or escalation.

Scope includes enterprise, campus, branch, data-center, cloud, hybrid, multicloud, Kubernetes, remote-access, edge, OT/IoT, AI/HPC, and real-time enterprise networks. It includes physical links, Ethernet, IP, routing, switching, DNS, DHCP, Wi-Fi, WAN, SD-WAN, VPN, firewalls, NAC, load balancers, cloud networking, container networking, observability, automation, lifecycle, resilience, and safe change behavior.

Exclude 3GPP RAN, EPC, 5GC, IMS, mobile spectrum planning, carrier OSS/BSS, carrier optical backbone, and service-provider core operations. Enterprise use of LTE or 5G as a WAN underlay is allowed.

The supplied domain, subdomain, task family, environment, vendor scope, incident mechanism, evidence condition, evidence bundle, action risk, difficulty, and presentation are mandatory constraints. Make them materially visible in the task rather than merely naming them.

Make the task concrete. Include a believable environment, symptom or requested change, affected scope, business or technical impact, and operational constraints. Use fictional organizations, documentation address ranges, and redacted identifiers. Never include real secrets.

Some tasks should contain enough focused evidence to reason from. Some should require the operator to request or call read-only tools. Some should intentionally omit required live state, current pricing, permissions, topology, or vendor behavior so that the correct behavior is to ask for evidence, abstain, stage, or escalate.

Never claim that a command, query, change, approval, rollback, or verification has already happened unless its result is included in the task. Never invent tool output, live state, current prices, approvals, or successful verification. Do not place the hidden answer in the prompt.

Machine data may be included when useful: short configuration excerpts, CLI output, routing tables, logs, alerts, metrics, flow records, packet summaries, IaC diffs, topology descriptions, incident timelines, runbook fragments, change records, or cloud-network artifacts. Keep excerpts focused; do not dump pages of irrelevant data.

For vendor-neutral tasks, use standards and generic operational language. For single-vendor tasks, use only real product concepts and syntax appropriate to the selected platform. For multi-vendor tasks, make the interoperability boundary operationally relevant. Do not invent commands, features, SKUs, or version behavior.

For any possible state-changing action, establish investigation first. Require a bounded and reversible change, approval when the supplied action risk requires it, prechecks, post-change verification, retained reachability, blast-radius awareness, and rollback. Production mutation must never be unrestricted.

Difficulty controls causal depth, ambiguity, number of devices and layers, evidence quality, vendor interactions, operational constraints, and safety risk:
- 1-3: one device or service, clear symptom, low ambiguity, low risk, existing runbook;
- 4-6: multiple plausible causes, more than one evidence source, multiple devices or layers, moderate operational risk;
- 7-8: multi-device, cross-layer or multi-vendor, conflicting evidence, production constraints, approval, blast-radius, rollback, or freeze concerns;
- 9-10: cross-domain or multi-fault, partial observability, long causal chain, stale state, safety conflict, absent runbook, or a correct need to abstain, stage, or escalate.

Match the supplied presentation: incident ticket, on-call chat, CLI or SSH session, configuration review, change request, war room, audit review, architecture review, or API/automation brief. Do not force every task into casual 2am chat.

Avoid multiple-choice questions, generic definitions, broad requests such as "troubleshoot the network," obvious answers, unrestricted production changes, and tasks that are only documentation memorization.

Output only the final user task prompt.
```

ATIF does not alter this prompt. ATIF is applied only after a teacher and harness have produced a trajectory. Adding serialization instructions here would leak pipeline concerns into the generated user task.

### 7.2 `prompts/netops-teacher-system-v1.txt`

The exact prompt is:

```text
You are a senior multi-vendor enterprise network operations teacher producing candidate operational trajectories.

Use only the user-visible prompt, supplied evidence, available tools, and returned tool results. Never fabricate live state, commands already executed, tool output, current prices, approvals, or successful verification.

For each decision:
- identify relevant observed facts and their evidence;
- distinguish facts from hypotheses;
- state important uncertainty;
- choose the smallest useful next action;
- prefer read-only inspection;
- explain what the selected action is expected to establish;
- interpret the returned result before continuing.

Do not modify network state until:
- evidence supports a bounded change;
- the action is within allowed tool and policy scope;
- required approval has been granted;
- preconditions and rollback are available.

After a change:
- independently verify the intended state;
- check regressions and retained reachability;
- roll back or escalate when verification fails.

Abstain or escalate when required live state, topology, permissions, current pricing, vendor behavior, or evidence is unavailable.

Expose concise operational rationale, evidence, hypotheses, uncertainty, actions, and results. Do not emit hidden or unnecessarily long chain-of-thought.

The harness, not you, supplies tool results, state hashes, ground truth, approval decisions, safety grades, verification status, and ATIF serialization.
```

The final ATIF sentence is the only ATIF-related prompt change. It is added to the teacher prompt, not the Taskgen prompt, to prevent the teacher from emitting an ATIF envelope or self-authored harness fields.

## 8. Task prompt record schema

`schemas/netops-task-v1.schema.json` is JSON Schema draft 2020-12. Required fields are:

```json
{
  "schema_version": "scogo.netops.task.v1",
  "prompt": "...",
  "domain": "enterprise_netops::layer3_routing",
  "subdomain": "bgp_route_leak",
  "difficulty": 8,
  "coordinates": {
    "taxonomy_id": "scogo-enterprise-netops-v1",
    "task_family": "troubleshooting_rca",
    "environment": "hybrid",
    "vendor_scope": "multi_vendor",
    "vendors": ["cisco_ios_xe", "juniper_junos"],
    "incident_mechanism": "misconfiguration",
    "evidence_condition": "contradictory",
    "evidence_bundle": "routing_tables",
    "action_risk": "read_only_investigation",
    "presentation": "war_room"
  },
  "taskgen_model": "scogoai/gpt-5.6-luna-max",
  "temperature": 0.9
}
```

`language` is optional and present only for multilingual generation. `additionalProperties` is false at the root and for `coordinates`.

This record intentionally has no prompt ID, split group, tools, results, evidence objects, approval, state hash, ground truth, outcome, reward, verification, safety grade, or acceptance decision. Those are downstream pipeline responsibilities.

## 9. Canonical teacher audit schema

`schemas/netops-teacher-trajectory-audit-v1.schema.json` is the source of truth for one completed or attempted teacher rollout. One JSON object is stored per JSONL line.

Required root fields:

| Field | Contract |
|---|---|
| `schema_version` | Constant `scogo.netops.teacher-trajectory.audit.v1`. |
| `record_kind` | `candidate`, `imported`, `accepted`, or `rejected`. |
| `sample_id` | Pipeline-assigned stable sample identifier. |
| `trajectory_id` | Unique identifier for this attempt. |
| `task` | Source prompt, prompt hash, taxonomy coordinates, difficulty, risk, and split group. |
| `generation` | Provider, teacher model and revision, sampling, prompt version, timestamp, and raw artifact reference. |
| `environment` | Mode, fixture/topology/reset references, allowed tools, and initial state hash. |
| `tools` | OpenAI-compatible function definitions available to the teacher. |
| `messages` | Ordered system, user, assistant, and tool messages. |
| `evidence` | Evidence registry with IDs, lineage, hashes, timestamps, and trainable excerpts. |
| `approval` | Whether approval was required, requested, granted, by whom, and for what scope. |
| `outcome` | Terminal status, cause, confidence, uncertainty, remediation, abstention, and escalation. |
| `verification` | Independent oracle checks, pre/post state, regression checks, and rollback result. |
| `safety` | Read-before-write, approval, prohibited action, destructive action, secret, and policy results. |
| `quality` | Schema, tool, grounding, terminal-claim, grader, acceptance, and rejection fields. |
| `provenance` | Taskgen run, source references, license review, contamination review, and content hash. |
| `interop` | Optional ATIF import/export preservation metadata. |

All root sections except `interop` are present on every record. A field that has not been evaluated is represented by `null`; collections use an empty array. This avoids treating omission as a successful check. Every object uses `additionalProperties: false`, except tool JSON Schemas and explicitly named `extra` or `original_atif` preservation objects.

Exact nested fields:

```text
task
  prompt_id: string
  prompt_sha256: 64-character lowercase hex string
  prompt: string
  taxonomy_id: string
  coordinates: same coordinate object as scogo.netops.task.v1
  difficulty: integer 1..10
  action_risk: taxonomy action-risk ID
  split_group_id: string

generation
  run_id: string
  provider: string
  teacher_model: string
  model_revision: string|null
  temperature: number|null
  seed: integer|null
  system_prompt_version: string
  generated_at: RFC 3339 timestamp
  raw_response_ref: string|null

environment
  mode: simulated|replay|authorized_live
  environment_id: string
  topology_ref: string|null
  fixture_ref: string|null
  reset_ref: string|null
  initial_state_sha256: 64-character lowercase hex string|null
  allowed_tool_names: string[]

messages[]
  message_id: string
  role: system|user|assistant|tool
  content: string or ATIF-compatible content-part array
  tool_calls: tool-call[]; assistant only
  tool_call_id: string|null; tool only
  evidence_refs: string[]
  timestamp: RFC 3339 timestamp|null

evidence[]
  evidence_id: string
  type: string
  source: string
  artifact_ref: string|null
  sha256: 64-character lowercase hex string|null
  observed_at: RFC 3339 timestamp|null
  excerpt: string|null
  generated_by: user|teacher|tool|harness|fixture
  sensitivity: public|synthetic|internal|restricted

approval
  required: boolean
  requested: boolean
  granted: boolean|null
  scope: string|null
  decision_source: string|null
  decided_at: RFC 3339 timestamp|null

outcome
  status: resolved|mitigated|planned|abstained|escalated|failed|unknown
  root_cause:
    summary: string|null
    entities: string[]
    evidence_refs: string[]
  confidence: number 0..1|null
  uncertainty: string[]
  remediation:
    planned: string[]
    executed: string[]
  verification_summary: string|null
  rollback_summary: string|null
  abstention_reason: string|null
  escalation_target: string|null

verification
  oracle: string|null
  checks[]:
    check_id: string
    description: string
    passed: boolean
    evidence_refs: string[]
  passed: boolean|null
  pre_state_sha256: 64-character lowercase hex string|null
  post_state_sha256: 64-character lowercase hex string|null
  regressions: string[]
  rollback_tested: boolean|null
  verifier: string|null

safety
  read_before_write: boolean|null
  write_without_approval: boolean
  prohibited_actions: string[]
  destructive_actions: string[]
  secrets_exposed: boolean
  policy_pass: boolean|null
  violations: string[]

quality
  schema_valid: boolean
  tool_calls_valid: boolean|null
  grounded: boolean|null
  terminal_claim_valid: boolean|null
  accepted: boolean
  rejection_reasons: string[]
  grader_refs: string[]

provenance
  taskgen_run_id: string
  source_prompt_ref: string
  source_refs: string[]
  license_review: approved|rejected|pending
  contamination_review: passed|failed|pending
  content_sha256: 64-character lowercase hex string

interop
  source_format: scogo_audit|atif
  source_schema_version: string
  original_atif: object|null
```

Canonical tool calls store JSON arguments as an object. The SFT projection serializes arguments according to the selected model's chat template; ATIF exports keep them as an object as required by ATIF.

Field ownership is normative:

| Owner | Fields |
|---|---|
| Taskgen | prompt text and sampled coordinates only |
| Teacher | assistant messages and tool-call requests |
| Harness | tool results and environment state hashes |
| Policy gate | approval decision |
| Fixture | hidden ground truth |
| Verifier | verification checks and state outcome |
| Policy grader | safety result |
| Independent grader or reviewer | semantic quality |
| Pipeline | identifiers, provenance, hashes, split, accept/reject |

Message rules:

- `role` is `system`, `user`, `assistant`, or `tool`;
- assistant messages may contain `tool_calls`;
- every tool message has a `tool_call_id` matching a prior unresolved assistant call;
- tool output is written only by the harness;
- evidence claims in assistant messages use `evidence_refs` that resolve in the evidence registry;
- concise visible rationale belongs in message content; hidden provider reasoning is excluded;
- state-changing calls require `approval.granted=true` and matching approval scope;
- final success claims require passed independent verification.

Outcome status is one of `resolved`, `mitigated`, `planned`, `abstained`, `escalated`, `failed`, or `unknown`. `unknown` is permitted only on external ATIF imports. Confidence is in `[0,1]` and is not a reward.

## 10. Trainable SFT projection schema

`schemas/netops-teacher-trajectory-sft-v1.schema.json` contains only accepted trainable content:

```json
{
  "schema_version": "scogo.netops.teacher-trajectory.sft.v1",
  "id": "sample-...",
  "trajectory_id": "trajectory-...",
  "messages": [],
  "tools": [],
  "metadata": {
    "taxonomy_id": "scogo-enterprise-netops-v1",
    "domain": "layer3_routing",
    "subdomain": "bgp_route_leak",
    "task_family": "troubleshooting_rca",
    "difficulty": 8,
    "split_group_id": "family-..."
  }
}
```

The SFT projection excludes:

- hidden ground truth and fixture internals;
- judge output, rewards, acceptance decisions, and rejection reasons;
- raw telemetry when a focused excerpt is sufficient;
- private environment IDs, secrets, credentials, and customer identifiers;
- raw provider hidden reasoning or long chain-of-thought;
- benchmark source identities;
- copied ATIF context where `is_copied_context=true`;
- deterministic agent steps where ATIF `llm_call_count=0`;
- unverified external ATIF imports.

The trainer masks system, user, and tool tokens and trains only assistant-authored outputs according to the target model's verified chat template.

## 11. ATIF v1.7 import/export

ATIF v1.7 is an interchange format, not the canonical audit schema. The implementation is pinned to `schema_version: ATIF-v1.7` and follows the active Harbor RFC current on 2026-08-19.

### 11.1 Canonical to ATIF

| Canonical field | ATIF field |
|---|---|
| `trajectory_id` | `trajectory_id` |
| `generation.run_id` when present | `session_id` |
| teacher identity and model | `agent.name`, `agent.version`, `agent.model_name` |
| canonical tools | `agent.tool_definitions` |
| system message | step with `source: system` |
| user message | step with `source: user` |
| assistant message | step with `source: agent` |
| assistant tool calls | agent-step `tool_calls` |
| matching harness tool messages | same agent-step `observation.results` |
| generation metrics | step `metrics` and `final_metrics` when available |
| audit-only Scogo fields | `extra.scogo` |

Export rules:

- step IDs start at 1 and are sequential;
- one LLM inference becomes one agent step and sets `llm_call_count: 1`;
- tool observations are attached to the agent step that issued the correlated call;
- tool-call IDs are unique and every `source_call_id` resolves;
- `reasoning_content` is omitted; concise operational rationale remains in `message`;
- Scogo approval, evidence, state hashes, verification, safety, quality, and provenance remain under `extra.scogo` for lossless Scogo round trips;
- no current price or cost is inferred. Cost fields are emitted only if captured at generation time;
- exported objects validate before being written.

### 11.2 ATIF to canonical

Import rules:

- validate the complete ATIF object before mapping it;
- support text and ATIF v1.6+ content-part arrays;
- map agent tool calls and observations back to assistant and tool messages;
- preserve `extra`, `reasoning_content`, copied-context markers, context-management events, continuations, and embedded subagents under `interop.original_atif` when they do not map to a trainable canonical field;
- restore Scogo audit fields from `extra.scogo` when present;
- assign `record_kind: imported`, `outcome.status: unknown`, and `quality.accepted: false` for external ATIF without trustworthy Scogo verifier metadata;
- add rejection reason `external_atif_unverified` until the trajectory is replayed or independently verified;
- never project imported hidden reasoning or copied context into SFT;
- reject unsupported ATIF versions rather than guessing compatibility.

ATIF imports are audit artifacts. Importing a file is never evidence that its actions succeeded or that it is trainable.

## 12. Errors and safety behavior

- Taxonomy parsing errors report the file, YAML path, and invalid value.
- Prompt-file errors occur before any API request.
- Output files are not created until taxonomy and prompt validation pass.
- JSONL conversion reports the line number and leaves the input untouched.
- ATIF conversion writes to a temporary sibling file and atomically renames it only after every record validates; a failed batch does not leave a partially trusted output.
- Existing output requires `--overwrite`; append is allowed only for generation JSONL, not conversion.
- Secrets are never logged. API keys and tool credentials remain outside all schemas.
- Imported ATIF `reasoning_content` is audit-only and never displayed in generated dataset cards.
- A state-changing trajectory without scoped approval or passed verification is rejected from SFT projection.

## 13. Testing contract

Unit tests:

- all taxonomy and difficulty distributions sum exactly within tolerance;
- 25 unique domains exist and every listed subdomain is unique within its domain;
- telecom-provider exclusions are present;
- every domain resolves at least one environment, vendor group, mechanism, and evidence bundle;
- seeded coordinate sampling is reproducible;
- difficulty bounds are honored per task family;
- `--system-prompt` and `--system-prompt-file` conflict;
- taxonomy-relative prompt paths resolve correctly;
- NetOps generation never receives the OEM addendum;
- task records validate against `netops-task-v1.schema.json`;
- canonical audit and SFT positive and negative fixtures validate;
- ATIF step IDs, tool-call references, copied context, metrics, content parts, and embedded subagent IDs validate;
- canonical -> ATIF -> canonical preserves Scogo fields;
- external ATIF imports remain unaccepted and non-trainable;
- hidden reasoning and copied context never enter SFT projection.

CLI integration tests:

- taxonomy validation succeeds for both checked-in taxonomy files;
- invalid weight, duplicate ID, unknown reference, missing prompt, and empty eligible-axis failures are actionable;
- one seeded mocked generation writes the expected coordinate record;
- single JSON and JSONL ATIF round trips work;
- malformed line conversion fails atomically;
- existing output is protected without `--overwrite`.

Repository verification:

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo run -- taxonomy validate --taxonomy docs/it-ops-taxonomy.yaml
cargo run -- taxonomy validate --taxonomy docs/netops-taxonomy.yaml
```

## 14. Documentation contract

`README.md` will document:

- the new `generate`, `atif`, and `taxonomy` subcommands;
- how to run the general IT Ops taxonomy and the dedicated NetOps taxonomy;
- prompt-file precedence;
- compositional coordinates and the exact output record;
- distribution and difficulty overrides;
- ATIF v1.7 import/export examples and the canonical-versus-interchange distinction;
- the task-seed, teacher, harness, verifier, audit, and SFT boundaries;
- the fact that ATIF import does not make a trajectory verified or trainable.

`docs/netops-data-contract.md` will provide one complete prompt record, canonical audit trajectory, SFT projection, and ATIF export of the same scenario.

## 15. Implementation boundaries

The first implementation does not:

- execute network tools;
- generate teacher trajectories;
- verify real devices;
- implement a network simulator;
- upload datasets;
- train or evaluate a model;
- add telecom-provider domains;
- add certification-question generation;
- automatically promote imported ATIF to SFT.

## 16. Source decisions

- ATIF support targets the current active [Harbor ATIF RFC v1.7](https://github.com/harbor-framework/harbor/blob/main/rfcs/0001-trajectory-format.md). The RFC defines sequential steps, tool calls and observations, metrics, copied-context filtering, and embedded subagent trajectories.
- Runtime YAML parsing will use the maintained Serde-compatible [`serde_yaml_ng`](https://docs.rs/serde_yaml_ng/latest/serde_yaml_ng/) crate rather than a custom parser.
- Scogo's canonical audit schema remains richer than ATIF because approval, evidence lineage, state verification, rollback, safety, acceptance, and provenance are first-class operational requirements.
