#!/bin/sh

. ../netifd-proto.sh

init_proto "$@"

proto_cryonet_init_config() {

	proto_config_add_string 'id'
	proto_config_add_string 'token'
	proto_config_add_string 'listen'
	proto_config_add_string 'servers'
	proto_config_add_string 'ice_servers'
	proto_config_add_string 'candidate_filter_prefixes:list(string)'
	proto_config_add_string 'tap_interface_name'
	proto_config_add_boolean 'encrypt_local_packets'
	proto_config_add_boolean 'enable_packet_information'
	proto_config_add_string 'ipaddr:ipaddr'
	proto_config_add_string 'ip6addr:ip6addr'
	no_device=1
	available=1
}

proto_cryonet_setup() {
	local config="$1"
	local id token listen servers ice_servers candidate_filter_prefixes
	local tap_interface_name encrypt_local_packets enable_packet_information ipaddr ip6addr
	json_get_vars id token listen servers ice_servers candidate_filter_prefixes \
		tap_interface_name encrypt_local_packets enable_packet_information ipaddr ip6addr

	[ -n "$id" ] || { proto_notify_error "$config" "missing id"; proto_block_restart "$config"; return 1; }
	case "$config" in
		''|*[!a-zA-Z0-9_]*) proto_notify_error "$config" INVALID_INTERFACE; proto_block_restart "$config"; return 1 ;;
	esac
	[ "${#config}" -le 60 ] || {
		proto_notify_error "$config" INVALID_INTERFACE
		proto_block_restart "$config"
		return 1
	}
	tap_interface_name="${tap_interface_name:-cn0}"
	case "$tap_interface_name" in
		*[!a-zA-Z0-9_.-]*|.|..)
			proto_notify_error "$config" INVALID_DEVICE
			proto_block_restart "$config"
			return 1
			;;
	esac
	[ "${#tap_interface_name}" -le 15 ] || {
		proto_notify_error "$config" INVALID_DEVICE
		proto_block_restart "$config"
		return 1
	}
	# Do not mistake an existing device (possibly another instance) for ours.
	[ ! -e "/sys/class/net/$tap_interface_name" ] || {
		proto_notify_error "$config" DEVICE_IN_USE
		proto_block_restart "$config"
		return 1
	}
	local ctl_path="/var/run/cryonet-$config.ctl"
	# A crashed daemon may leave a socket behind. Only a fresh socket counts
	# as ready: Cryonet binds it after initializing the TAP and other managers.
	rm -f "$ctl_path" || {
		proto_notify_error "$config" SOCKET_CLEANUP_FAILED
		return 1
	}
	set -- /usr/bin/cryonet "$id" --tap-mode --ctl-path "/var/run/cryonet-$config.ctl"
	[ -n "$token" ] && set -- "$@" --token "$token"
	[ -n "$listen" ] && set -- "$@" --listen "$listen"
	[ -n "$servers" ] && set -- "$@" --servers "$servers"
	[ -n "$ice_servers" ] && set -- "$@" --ice-servers "$ice_servers"
	for prefix in $candidate_filter_prefixes; do
		set -- "$@" --candidate-filter-prefixes "$prefix"
	done
	[ -n "$tap_interface_name" ] && set -- "$@" --tap-interface-name "$tap_interface_name"
	[ "$encrypt_local_packets" = 1 ] && set -- "$@" --encrypt-local-packets
	[ "$enable_packet_information" = 1 ] && set -- "$@" --enable-packet-information

	proto_run_command "$config" "$@" || {
		proto_notify_error "$config" START_FAILED
		return 1
	}
	# netifd supervises the daemon and aborts setup if it exits or ifdown is
	# requested. Bound the wait as a live daemon can also stall during startup.
	local remaining=10
	while [ "$remaining" -gt 0 ]; do
		if [ -e "/sys/class/net/$tap_interface_name" ] && [ -S "$ctl_path" ]; then
			proto_init_update "$tap_interface_name" 1
			[ -n "$ipaddr" ] && proto_add_ipv4_address "${ipaddr%/*}" "${ipaddr#*/}"
			[ -n "$ip6addr" ] && proto_add_ipv6_address "${ip6addr%/*}" "${ip6addr#*/}"
			proto_send_update "$config"
			return $?
		fi
		sleep 1
		remaining=$((remaining - 1))
	done
	proto_notify_error "$config" START_TIMEOUT
	proto_kill_command "$config"
	return 1
}

proto_cryonet_teardown() {
	local config="$1"
	proto_kill_command "$config"
	proto_init_update '*' 0
	proto_send_update "$config"
}

add_protocol cryonet
