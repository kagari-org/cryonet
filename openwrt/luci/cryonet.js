'use strict';
'require form';
'require network';
'require uci';

network.registerErrorCode('INVALID_INTERFACE', _('Use an interface section name containing 1–60 ASCII letters, digits or underscores.'));
network.registerErrorCode('INVALID_DEVICE', _('Invalid TAP device name.'));
network.registerErrorCode('DEVICE_IN_USE', _('The TAP device already exists. Choose another name or stop its owner.'));
network.registerErrorCode('SOCKET_CLEANUP_FAILED', _('Unable to remove the previous control socket.'));
network.registerErrorCode('START_FAILED', _('Unable to start Cryonet.'));
network.registerErrorCode('START_TIMEOUT', _('Cryonet did not create its TAP device and control socket within 10 seconds.'));

return network.registerProtocol('cryonet', {
	getI18n() {
		return _('Cryonet (TAP)');
	},

	getIfname() {
		return this._ubus('l3_device') || uci.get('network', this.sid, 'tap_interface_name') || 'cn0';
	},

	getPackageName() {
		return 'cryonet';
	},

	isFloating() {
		return true;
	},

	isVirtual() {
		return true;
	},

	getDevices() {
		return null;
	},

	getDevice() {
		return network.instantiateDevice(this.getIfname(), this);
	},

	containsDevice(ifname) {
		return network.getIfnameOf(ifname) === this.getIfname();
	},

	renderFormOptions(s) {
		let o;

		o = s.taboption('general', form.Value, 'token', _('Token'));
		o.password = true;
		o.rmempty = true;

		o = s.taboption('general', form.Value, 'listen', _('Listen address'),
			_('IPv4:port or [IPv6]:port. Use different listening endpoints for multiple instances.'));
		o.placeholder = '0.0.0.0:2333';
		o.rmempty = true;

		o = s.taboption('general', form.Value, 'servers', _('Servers'),
			_('Comma-separated server addresses, using the same format as the Cryonet command line.'));
		o.rmempty = true;

		o = s.taboption('general', form.Value, 'tap_interface_name', _('TAP device name'),
			_('Cryonet creates one TAP device. Use a different device name for each instance. Leave empty to use cn0.'));
		o.placeholder = 'cn0';
		o.rmempty = true;
		o.validate = (section_id, value) => {
			return !value || (/^[a-zA-Z0-9_.-]{1,15}$/.test(value) && value !== '.' && value !== '..')
				? true : _('Use 1–15 ASCII letters, digits, underscores, dots or hyphens. Single-dot and double-dot names are not allowed.');
		};

		o = s.taboption('general', form.Value, 'ipv6_subnet', _('IPv6 subnet'),
			_('Optional /64 IPv6 subnet, for example fd00:1234:5678:9abc::/64 or 2001:db8:1:2::/64. The node ID is appended as the interface identifier.'));
		o.datatype = 'cidr6';
		o.rmempty = true;
		o.validate = (section_id, value) => {
			return !value || (/\/64$/.test(value))
				? true : _('Enter an IPv6 /64 subnet, for example fd00::/64.');
		};

		o = s.taboption('general', form.Value, 'ipaddr', _('IPv4 address'),
			_('Optional address in CIDR notation, for example 10.11.0.254/24.'));
		o.datatype = 'cidr4';
		o.rmempty = true;

		o = s.taboption('general', form.Value, 'ip6addr', _('IPv6 address'),
			_('Optional address in CIDR notation, for example fd00::254/64.'));
		o.datatype = 'cidr6';
		o.rmempty = true;

		o = s.taboption('advanced', form.Value, 'ice_servers', _('ICE servers'),
			_('Comma-separated STUN/TURN entries: URL or URL|username|credential.'));
		o.password = true;
		o.rmempty = true;

		o = s.taboption('advanced', form.DynamicList, 'candidate_filter_prefixes', _('Candidate filter prefixes'),
			_('Optional IPv4 or IPv6 CIDR prefixes. Candidates matching any of these prefixes are filtered out.'));
		o.datatype = 'or(cidr4,cidr6)';
		o.rmempty = true;

		o = s.taboption('advanced', form.Flag, 'encrypt_local_packets', _('Encrypt local packets'));
		o.default = o.disabled;
		o.rmempty = false;

		o = s.taboption('advanced', form.Flag, 'enable_packet_information', _('Enable packet information'));
		o.default = o.disabled;
		o.rmempty = false;
	}
});
