# -*- coding: utf-8 -*-
# Copyright (c) 2020 DDN. All rights reserved.
# Use of this source code is governed by a MIT-style
# license that can be found in the LICENSE file.

import logging

from django.db import models
from django.db.models import CASCADE
from chroma_core.models import AlertStateBase
from chroma_core.models import AlertEvent
from chroma_core.models import DeletableStatefulObject
from chroma_core.models import StateChangeJob
from chroma_core.models import Job
from chroma_core.models import SchedulingError
from chroma_core.models import StateLock
from chroma_core.lib.job import DependOn, DependAll, Step
from chroma_help.help import help_text


class PacemakerConfiguration(DeletableStatefulObject):
    states = ["unconfigured", "stopped", "started"]
    initial_state = "unconfigured"

    host = models.OneToOneField("ManagedHost", related_name="_pacemaker_configuration", on_delete=CASCADE)

    def __str__(self):
        return "%s Pacemaker configuration" % self.host

    class Meta:
        app_label = "chroma_core"
        ordering = ["id"]

    def get_label(self):
        return "pacemaker configuration"

    def set_state(self, state, intentional=False):
        """
        :param intentional: set to true to silence any alerts generated by this transition
        """
        super(PacemakerConfiguration, self).set_state(state, intentional)
        if intentional:
            PacemakerStoppedAlert.notify_warning(self, self.state != "started")
        else:
            PacemakerStoppedAlert.notify(self, self.state != "started")

    reverse_deps = {"ManagedHost": lambda mh: PacemakerConfiguration.objects.filter(host_id=mh.id)}

    # Below the handler should be in a completion hook, but I can't see how to get the instance of the completion
    # hook to add it and time is running out. I will return to this.

    @property
    def reconfigure_fencing(self):
        # We return False because we are overloading the attribute setter below to make it a event handler rather than
        # a real property. If some sets reconfigure_fencing = False then the event will not be called because the current
        # value is always False. If someone sets reconfigure_fencing = True then the setter will be called because the
        # current value is always False!
        return False

    @reconfigure_fencing.setter
    def reconfigure_fencing(self, ignored_value):
        # We don't store this because we are overloading the attribute setter below to make it a event handler rather than
        # a real property.
        pass


class StonithNotEnabledAlert(AlertStateBase):
    default_severity = logging.ERROR

    class Meta:
        app_label = "chroma_core"
        proxy = True

    def alert_message(self):
        return help_text["stonith_not_enabled"] % self.alert_item

    def end_event(self):
        return AlertEvent(
            message_str=help_text["stonith_enabled"] % self.alert_item,
            alert_item=self.alert_item,
            alert=self,
            severity=logging.INFO,
        )

    @property
    def affected_objects(self):
        """
        :return: A list of objects that are affected by this alert
        """
        return [self.alert_item.host]


class PacemakerStoppedAlert(AlertStateBase):
    # Pacemaker being down is never solely responsible for a filesystem
    # being unavailable: if a target is offline we will get a separate
    # ERROR alert for that.  Pacemaker being offline may indicate a configuration
    # fault, but equally could just indicate that the host hasn't booted up that far yet.
    default_severity = logging.INFO

    def alert_message(self):
        return "Pacemaker stopped on server %s" % self.alert_item.host

    class Meta:
        app_label = "chroma_core"
        proxy = True

    def end_event(self):
        return AlertEvent(
            message_str="Pacemaker started on server '%s'" % self.alert_item.host,
            alert_item=self.alert_item.host,
            alert=self,
            severity=logging.WARNING,
        )

    @property
    def affected_objects(self):
        """
        :return: A list of objects that are affected by this alert
        """
        return [self.alert_item.host]


class ConfigurePacemakerStep(Step):
    idempotent = True

    def run(self, kwargs):
        host = kwargs["host"]
        self.invoke_agent_expect_result(host, "configure_pacemaker")


class ConfigurePacemakerJob(StateChangeJob):
    state_transition = StateChangeJob.StateTransition(PacemakerConfiguration, "unconfigured", "stopped")
    stateful_object = "pacemaker_configuration"
    pacemaker_configuration = models.ForeignKey(PacemakerConfiguration, on_delete=CASCADE)
    state_verb = "Configure Pacemaker"

    display_group = Job.JOB_GROUPS.COMMON
    display_order = 30

    class Meta:
        app_label = "chroma_core"
        ordering = ["id"]

    @classmethod
    def long_description(cls, stateful_object):
        return help_text["configure_pacemaker"]

    def description(self):
        return help_text["configure_pacemaker_on"] % self.pacemaker_configuration.host

    def get_steps(self):
        return [
            (StartPacemakerStep, {"host": self.pacemaker_configuration.host}),
            (ConfigurePacemakerStep, {"host": self.pacemaker_configuration.host}),
            (StopPacemakerStep, {"host": self.pacemaker_configuration.host}),
        ]

    def get_deps(self):
        """
        Before Pacemaker operations are possible the host must have had its packages installed.
        Maybe we need a packages object, but this routine at least keeps the detail in one place.

        Also corosync needs to be up and running. This is because configuring pacemaker requires starting pacemaker.

        Or maybe we need an unacceptable_states lists.
        :return:
        """
        if self.pacemaker_configuration.host.state in ["unconfigured", "undeployed"]:
            deps = [DependOn(self.pacemaker_configuration.host, "packages_installed")]
        else:
            deps = []

        deps.append(DependOn(self.pacemaker_configuration.host.corosync_configuration, "started"))

        return DependAll(deps)


class UnconfigurePacemakerStep(Step):
    idempotent = True

    def run(self, kwargs):
        host = kwargs["host"]
        self.invoke_agent_expect_result(host, "unconfigure_pacemaker")


class UnconfigurePacemakerJob(StateChangeJob):
    state_transition = StateChangeJob.StateTransition(PacemakerConfiguration, "stopped", "unconfigured")
    stateful_object = "pacemaker_configuration"
    pacemaker_configuration = models.ForeignKey(PacemakerConfiguration, on_delete=CASCADE)
    state_verb = "Unconfigure Pacemaker"

    display_group = Job.JOB_GROUPS.COMMON
    display_order = 30

    class Meta:
        app_label = "chroma_core"
        ordering = ["id"]

    @classmethod
    def long_description(cls, stateful_object):
        return help_text["unconfigure_pacemaker"]

    def description(self):
        return help_text["unconfigure_pacemaker_on"] % self.pacemaker_configuration.host

    def get_steps(self):
        # Sadly we need to restart and then stop (it will be stopped) pacemaker to configure it.
        # It will be stopped because this transition is stopped->unconfigured.
        return [
            (StartPacemakerStep, {"host": self.pacemaker_configuration.host}),
            (UnconfigurePacemakerStep, {"host": self.pacemaker_configuration.host}),
            (StopPacemakerStep, {"host": self.pacemaker_configuration.host}),
        ]

    def get_deps(self):
        """
        Before Pacemaker operations are possible the host must have had its packages installed.
        Maybe we need a packages object, but this routine at least keeps the detail in one place.

        Also corosync needs to be up and running. This is because configuring pacemaker requires starting pacemaker.

        Or maybe we need an unacceptable_states lists.
        :return:
        """
        if self.pacemaker_configuration.host.state in ["unconfigured", "undeployed"]:
            deps = [DependOn(self.pacemaker_configuration.host, "packages_installed")]
        else:
            deps = []

        deps.append(DependOn(self.pacemaker_configuration.host.corosync_configuration, "started"))

        # Any targets will have to be removed.
        from chroma_core.models.target import get_host_targets

        for t in get_host_targets(self.pacemaker_configuration.host.id):
            deps.append(DependOn(target, "removed"))

        return DependAll(deps)

    @classmethod
    def can_run(cls, instance):
        """We don't want people to unconfigure pacemaker on a node that has a target so make the command
        available only when that is not the case.
        :param instance: PacemakerConfiguration instance being queried
        :return: True if no target exist on the host in question.
        """
        from chroma_core.models.target import get_host_targets

        return len(get_host_targets(instance.host.id)) == 0


class StartPacemakerStep(Step):
    idempotent = True

    def run(self, kwargs):
        self.invoke_agent_expect_result(kwargs["host"], "start_pacemaker")

    @classmethod
    def describe(cls, kwargs):
        return help_text["start_pacemaker_on"] % kwargs["host"].fqdn


class StartPacemakerJob(StateChangeJob):
    state_transition = StateChangeJob.StateTransition(PacemakerConfiguration, "stopped", "started")
    stateful_object = "pacemaker_configuration"
    pacemaker_configuration = models.ForeignKey(PacemakerConfiguration, on_delete=CASCADE)
    state_verb = "Start Pacemaker"

    display_group = Job.JOB_GROUPS.COMMON
    display_order = 30

    class Meta:
        app_label = "chroma_core"
        ordering = ["id"]

    @classmethod
    def long_description(cls, stateful_object):
        return help_text["start_pacemaker"]

    def description(self):
        return "Start Pacemaker on %s" % self.pacemaker_configuration.host

    def get_steps(self):
        return [(StartPacemakerStep, {"host": self.pacemaker_configuration.host})]

    def get_deps(self):
        return DependOn(self.pacemaker_configuration.host.corosync_configuration, "started")


class StopPacemakerStep(Step):
    idempotent = True

    def run(self, kwargs):
        self.invoke_agent_expect_result(kwargs["host"], "stop_pacemaker")

    @classmethod
    def describe(cls, kwargs):
        return help_text["stop_pacemaker_on"] % kwargs["host"].fqdn


class StopPacemakerJob(StateChangeJob):
    state_transition = StateChangeJob.StateTransition(PacemakerConfiguration, "started", "stopped")
    stateful_object = "pacemaker_configuration"
    pacemaker_configuration = models.ForeignKey(PacemakerConfiguration, on_delete=CASCADE)
    state_verb = "Stop Pacemaker"

    display_group = Job.JOB_GROUPS.RARE
    display_order = 100

    class Meta:
        app_label = "chroma_core"
        ordering = ["id"]

    @classmethod
    def long_description(cls, stateful_object):
        return help_text["stop_pacemaker"]

    def description(self):
        return "Stop Pacemaker on %s" % self.pacemaker_configuration.host

    def get_steps(self):
        return [(StopPacemakerStep, {"host": self.pacemaker_configuration.host})]


class GetPacemakerStateStep(Step):
    idempotent = True

    # FIXME: using database=True to do the alerting update inside .set_state but
    # should do it in a completion
    database = True

    def run(self, kwargs):
        from chroma_core.services.job_scheduler.agent_rpc import AgentException

        host = kwargs["host"]

        try:
            lnet_data = self.invoke_agent(host, "device_plugin", {"plugin": "linux_network"})["linux_network"]["lnet"]
            host.set_state(lnet_data["state"])
            host.save(update_fields=["state", "state_modified_at"])
        except TypeError:
            self.log("Data received from old client. Host %s state cannot be updated until agent is updated" % host)
        except AgentException as e:
            self.log("No data for plugin linux_network from host %s due to exception %s" % (host, e))


class GetPacemakerStateJob(Job):
    pacemaker_configuration = models.ForeignKey(PacemakerConfiguration, on_delete=CASCADE)
    requires_confirmation = False
    verb = "Get Pacemaker state"

    class Meta:
        app_label = "chroma_core"
        ordering = ["id"]

    def create_locks(self):
        return [StateLock(job=self, locked_item=self.pacemaker_configuration, write=True)]

    @classmethod
    def get_args(cls, pacemaker_configuration):
        return {"host": pacemaker_configuration.host}

    @classmethod
    def long_description(cls, stateful_object):
        return help_text["pacemaker_state"]

    def description(self):
        return "Get Pacemaker state for %s" % self.pacemaker_configuration.host

    def get_steps(self):
        return [(GetPacemakerStateStep, {"host": self.pacemaker_configuration.host})]


class ConfigureHostFencingJob(Job):
    host = models.ForeignKey("ManagedHost", on_delete=CASCADE)
    requires_confirmation = False
    verb = "Configure Host Fencing"

    class Meta:
        app_label = "chroma_core"
        ordering = ["id"]

    @classmethod
    def get_args(cls, host):
        return {"host_id": host.id}

    @classmethod
    def long_description(cls, stateful_object):
        return help_text["configure_host_fencing"]

    def description(self):
        return "Configure fencing agent on %s" % self.host

    def create_locks(self):
        return [StateLock(job=self, locked_item=self.host.pacemaker_configuration, write=True)]

    def get_steps(self):
        return [(ConfigureHostFencingStep, {"host": self.host})]


class ConfigureHostFencingStep(Step):
    idempotent = True
    # Needs database in order to query host outlets
    database = True

    def run(self, kwargs):
        host = kwargs["host"]

        if host.state != "managed":
            raise SchedulingError(
                "Attempted to configure a fencing device while the host %s was in state %s. Expected host to be in state 'managed'. Please ensure your host has completed set up and configure power control again."
                % (host.fqdn, host.state)
            )

        if not host.pacemaker_configuration:
            # Shouldn't normally happen, but makes debugging our own bugs easier.
            raise RuntimeError(
                "Attemped to configure fencing on a host that does not yet have a pacemaker configuration."
            )

        agent_kwargs = []
        for outlet in host.outlets.select_related().all():
            fence_kwargs = {
                "agent": outlet.device.device_type.agent,
                "login": outlet.device.username,
                "password": outlet.device.password,
            }
            # IPMI fencing config doesn't need most of these attributes.
            if outlet.device.is_ipmi and outlet.device.device_type.agent not in ["fence_virsh", "fence_vbox"]:
                fence_kwargs["ipaddr"] = outlet.identifier
                fence_kwargs["lanplus"] = "2.0" in outlet.device.device_type.model  # lanplus
            else:
                fence_kwargs["plug"] = outlet.identifier
                fence_kwargs["ipaddr"] = outlet.device.address
                fence_kwargs["ipport"] = outlet.device.port

            agent_kwargs.append(fence_kwargs)

        self.invoke_agent(host, "configure_fencing", {"agents": agent_kwargs})
