#
# INTEL CONFIDENTIAL
#
# Copyright 2013-2014 Intel Corporation All Rights Reserved.
#
# The source code contained or described herein and all documents related
# to the source code ("Material") are owned by Intel Corporation or its
# suppliers or licensors. Title to the Material remains with Intel Corporation
# or its suppliers and licensors. The Material contains trade secrets and
# proprietary and confidential information of Intel or its suppliers and
# licensors. The Material is protected by worldwide copyright and trade secret
# laws and treaty provisions. No part of the Material may be used, copied,
# reproduced, modified, published, uploaded, posted, transmitted, distributed,
# or disclosed in any way without Intel's prior express written permission.
#
# No license under any patent, copyright, trade secret or other intellectual
# property right is granted to or conferred upon you by disclosure or delivery
# of the Materials, either expressly, by implication, inducement, estoppel or
# otherwise. Any license under such intellectual property rights must be
# express and approved by Intel in writing.


from polymorphic.models import DowncastMetaclass

from django.db import models
from django.contrib.contenttypes.models import ContentType
from django.contrib.contenttypes.generic import GenericForeignKey

from chroma_core.models import utils as conversion_util


class Event(models.Model):
    __metaclass__ = DowncastMetaclass

    created_at = models.DateTimeField(auto_now_add = True)
    severity = models.IntegerField(help_text = ("String indicating the "
                                                "severity of the event, "
                                                "one of %s") %
                                        conversion_util.STR_TO_SEVERITY.keys())

    host = models.ForeignKey('chroma_core.ManagedHost', blank = True, null = True)
    dismissed = models.BooleanField(default=False,
                                    help_text = "``true`` denotes that the "
                                                "user has acknowledged this "
                                                "event.")

    class Meta:
        app_label = 'chroma_core'
        ordering = ['id']

    @staticmethod
    def type_name():
        raise NotImplementedError

    def message(self):
        raise NotImplementedError


class LearnEvent(Event):
    # Every environment at some point reinvents void* :-)
    learned_item_type = models.ForeignKey(ContentType)
    learned_item_id = models.PositiveIntegerField()
    learned_item = GenericForeignKey('learned_item_type', 'learned_item_id')

    class Meta:
        app_label = 'chroma_core'
        ordering = ['id']

    @staticmethod
    def type_name():
        return "Autodetection"

    def message(self):
        from chroma_core.models import ManagedTarget, ManagedFilesystem, ManagedTargetMount
        if isinstance(self.learned_item, ManagedTargetMount):
            return "Discovered mount point of %s on %s" % (self.learned_item, self.learned_item.host)
        elif isinstance(self.learned_item, ManagedTarget):
            return "Discovered formatted target %s" % self.learned_item
        elif isinstance(self.learned_item, ManagedFilesystem):
            return "Discovered filesystem %s on MGS %s" % (self.learned_item, self.learned_item.mgs.primary_host)
        else:
            return "Discovered %s" % self.learned_item


class AlertEvent(Event):
    message_str = models.CharField(max_length = 512)
    alert = models.ForeignKey('AlertState')

    class Meta:
        app_label = 'chroma_core'
        ordering = ['id']

    @staticmethod
    def type_name():
        return "Alert"

    def message(self):
        return self.message_str


class SyslogEvent(Event):
    message_str = models.CharField(max_length = 512)
    lustre_pid = models.IntegerField(null = True)

    class Meta:
        app_label = 'chroma_core'
        ordering = ['id']

    @staticmethod
    def type_name():
        return "Syslog"

    def message(self):
        return self.message_str


class ClientConnectEvent(Event):
    message_str = models.CharField(max_length = 512)
    lustre_pid = models.IntegerField(null = True)

    class Meta:
        app_label = 'chroma_core'
        ordering = ['id']

    def message(self):
        return self.message_str

    @staticmethod
    def type_name():
        return "ClientConnect"
